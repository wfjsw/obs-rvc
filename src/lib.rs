mod rvc;
mod rt_utils;

use ndarray::s;
use parking_lot::{Condvar, FairMutex, Mutex};
use rubato::{FftFixedInOut, Resampler};
use rvc::RvcInfer;
use rt_utils::{get_sola_offset, upmix_audio_data, upmix_audio_data_context};

use obs_wrapper::{media::audio, obs_register_module, obs_string, prelude::*, properties::{NumberProp, PathProp, PathType, Properties}, source::*};

use std::{borrow::{BorrowMut, Cow}, cell::RefCell, collections::VecDeque, f32::consts::PI, panic, path::PathBuf, rc::Rc, sync::{atomic::AtomicBool, Arc}, thread::JoinHandle, time::{self, Duration, Instant}};

use crate::rt_utils::downmix_to_mono;

macro_rules! get_path_from_settings {
    ($settings:ident, $setting:ident) => {
        if let Some(path) = $settings.get::<Cow<str>>($setting) {
            if !path.is_empty() {
                let path = PathBuf::from(path.to_string());
                if path.exists() {
                    Some(path)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        }
    };

    ($field:expr, $settings:ident, $setting:ident) => {
        if let Some(path) = $settings.get::<Cow<str>>($setting) {
            let new_path_str = path.to_string();
            let orig_path = $field.as_ref().map(|p| p.to_str().unwrap_or(""));
            if $field.is_none() || orig_path != Some(new_path_str.as_str()) {
                if !path.is_empty() {
                    let path = PathBuf::from(path.to_string());
                    if path.exists() {
                        $field = Some(path);
                    } else {
                        $field = None;
                    }
                    true
                } else {
                    $field = None;
                    true
                }
            } else {
                false
            }
        } else {
            false
        }
    };
}

const SETTING_MODEL_PATH: ObsString = obs_string!("model_path");
const SETTING_INDEX_PATH: ObsString = obs_string!("index_path");
const SETTING_PITCH_SHIFT: ObsString = obs_string!("pitch_shift");
const SETTING_RESONANCE_SHIFT: ObsString = obs_string!("resonance_shift");
const SETTING_INDEX_RATE: ObsString = obs_string!("index_rate");
const SETTING_LOUDNESS_FACTOR: ObsString = obs_string!("loudness_factor");
// const SETTING_PITCH_ALGORITHM: ObsString = obs_string!("pitch_algorithm");
const SETTING_SAMPLE_LENGTH: ObsString = obs_string!("sample_length");
const SETTING_FADE_LENGTH: ObsString = obs_string!("fade_length");
const SETTING_EXTRA_INFERENCE_TIME: ObsString = obs_string!("extra_inference_time");
const SETTING_DEST_SAMPLE_RATE: ObsString = obs_string!("dest_sample_rate");

struct RvcInferenceState {
    model_path: Option<PathBuf>,
    index_path: Option<PathBuf>,
    model_output_sample_rate: usize,
    pitch_shift: i32,
    resonance_shift: f64,
    index_rate: f64,
    rms_mix_rate: f64,
    sample_length: f64,
    crossfade_length: f64,
    extra_inference_time: f64,

    sample_rate: usize,

    sample_frame_size: usize,
    sample_frame_16k: usize,
    crossfade_frame_size: usize,
    sola_buffer_frame_size: usize,
    sola_search_frame_size: usize,
    extra_frame_size: usize,
    model_return_length: usize,
    model_return_size: usize,

    input_buffer: Vec<f32>,
    input_buffer_16k: Vec<f32>,
    sola_buffer: ndarray::Array1<f32>,
    output_buffer: Vec<f32>,

    fade_in_window: ndarray::Array1<f32>,
    fade_out_window: ndarray::Array1<f32>,

    upsampler: FftFixedInOut<f32>,
    downsampler: FftFixedInOut<f32>,

    engine: RvcInfer,
}

struct RvcInferenceSharedState {
    state: FairMutex<RvcInferenceState>,
    running: AtomicBool,
    channels: usize,
    input: Mutex<VecDeque<f32>>,
    output: Mutex<VecDeque<f32>>,
    timestamps: Mutex<VecDeque<u64>>,
    has_input: Condvar,
    buffer_changed: AtomicBool,
}

struct RvcInferenceFilter {
    thread_handle: Option<JoinHandle<()>>,
    shared_state: Arc<RvcInferenceSharedState>,
}

struct RvcInferenceModule {
    context: ModuleRef,
}



impl Sourceable for RvcInferenceFilter {
    fn get_id() -> ObsString {
        obs_string!("rvc_inference_filter")
    }
    fn get_type() -> SourceType {
        SourceType::Filter
    }
    fn create(create: &mut CreatableSourceContext<Self>, _source: SourceRef) -> Self {
        let (sample_rate, channels) =
            create.with_audio(|audio| (audio.sample_rate(), audio.channels()));

        let settings = &mut create.settings;

        let model_path = get_path_from_settings!(settings, SETTING_MODEL_PATH);
        let index_path = get_path_from_settings!(settings, SETTING_INDEX_PATH);

        let mut rvc = RvcInfer::new();

        if let Some(model_path) = model_path.clone() {
            match rvc.load_model(model_path) {
                Ok(_) => (),
                Err(e) => {
                    println!("Error loading model: {:?}", e);
                }
            }
        }

        settings.set_default::<i32>(SETTING_DEST_SAMPLE_RATE, 40000);
        settings.set_default::<i32>(SETTING_PITCH_SHIFT, 12);
        settings.set_default::<f32>(SETTING_RESONANCE_SHIFT, 0.07);
        settings.set_default::<f32>(SETTING_INDEX_RATE, 0.0);
        settings.set_default::<f32>(SETTING_LOUDNESS_FACTOR, 0.5);
        settings.set_default::<f32>(SETTING_SAMPLE_LENGTH, 0.30);
        settings.set_default::<f32>(SETTING_FADE_LENGTH, 0.07);
        settings.set_default::<f32>(SETTING_EXTRA_INFERENCE_TIME, 2.00);

        let model_output_sample_rate = settings.get(SETTING_DEST_SAMPLE_RATE).unwrap_or(40000);
        let sample_length = settings.get(SETTING_SAMPLE_LENGTH).unwrap_or(0.30);
        let crossfade_length = settings.get(SETTING_FADE_LENGTH).unwrap_or(0.07);
        let extra_inference_time = settings.get(SETTING_EXTRA_INFERENCE_TIME).unwrap_or(2.00);

        let zc = sample_rate / 100;

        let sample_frame_time = (sample_length * sample_rate as f64 / zc as f64).round() as usize;
        let sample_frame_size = sample_frame_time * zc;

        let sample_frame_16k = sample_frame_time * 160;
        let crossfade_frame_size = (crossfade_length * sample_rate as f64 / zc as f64).round() as usize * zc;
        let sola_buffer_frame_size = usize::min(crossfade_frame_size, 4 * zc);
        let sola_search_frame_size = zc;
        let extra_frame_size = (extra_inference_time * sample_rate as f64 / zc as f64).round() as usize * zc;

        // println!("sample rate: {:?}", sample_rate);
        // println!("extra frame size: {:?}", extra_frame_size);
        // println!("crossfade frame size: {:?}", crossfade_frame_size);
        // println!("sola search frame size: {:?}", sola_search_frame_size);
        // println!("sample frame size: {:?}", sample_frame_size);

        let input_buffer_size = extra_frame_size + crossfade_frame_size + sola_search_frame_size + sample_frame_size;
        let input_buffer = vec![0_f32; input_buffer_size];

        let input_buffer_16k_size = 160 * input_buffer_size / zc;
        let input_buffer_16k = vec![0_f32; input_buffer_16k_size];

        let model_return_length = (sample_frame_size + sola_buffer_frame_size + sola_search_frame_size) / zc;
        let model_return_size = model_return_length * (model_output_sample_rate / 100);

        let sola_buffer = ndarray::Array1::zeros(sola_buffer_frame_size);

        let mut fade_in_window = ndarray::Array1::linspace(0.0, 1.0, sola_buffer_frame_size);
        fade_in_window.mapv_inplace(|x| f32::sin(x * 0.5 * PI).powi(2));
        let fade_out_window = fade_in_window.mapv(|x| 1.0 - x);

        // model_sample_size => 48k
        let upsampler = FftFixedInOut::new(model_output_sample_rate, sample_rate, model_return_size, 1).unwrap();
        
        let output_buffer = vec![0_f32; upsampler.output_frames_max()];

        // 48k => 16k sample frame size
        let downsampler = FftFixedInOut::new(sample_rate, 16000, sample_frame_size, 1).unwrap();

        let state = RvcInferenceState {
            sample_rate,

            model_path,
            index_path,
            model_output_sample_rate,
            pitch_shift: settings.get(SETTING_PITCH_SHIFT).unwrap_or(12),
            resonance_shift: settings.get(SETTING_RESONANCE_SHIFT).unwrap_or(0.00),
            index_rate: settings.get(SETTING_INDEX_RATE).unwrap_or(0.00),
            rms_mix_rate: settings.get(SETTING_LOUDNESS_FACTOR).unwrap_or(0.00),
            sample_length,
            crossfade_length,
            extra_inference_time,

            sample_frame_size,
            sample_frame_16k,
            crossfade_frame_size,
            sola_buffer_frame_size,
            sola_search_frame_size,
            extra_frame_size,
            model_return_length,
            model_return_size,

            input_buffer,
            input_buffer_16k,
            sola_buffer,
            output_buffer,

            fade_in_window,
            fade_out_window,

            upsampler,
            downsampler,

            engine: rvc,
        };

        let state = FairMutex::new(state);

        let shared_state = RvcInferenceSharedState {
            state,
            running: AtomicBool::new(true),
            channels,
            input: Mutex::new(VecDeque::with_capacity(sample_frame_size * 16)),
            output: Mutex::new(VecDeque::with_capacity(sample_frame_size * 16)),
            timestamps: Mutex::new(VecDeque::with_capacity(sample_frame_size * 16)),
            has_input: Condvar::new(),
            buffer_changed: AtomicBool::new(false),
        };

        let shared_state = Arc::new(shared_state);

        Self {
            thread_handle: None,
            shared_state,
        }
    }
}

impl GetNameSource for RvcInferenceFilter {
    fn get_name() -> ObsString {
        obs_string!("Retrieval Voice Conversion")
    }
}



impl GetPropertiesSource for RvcInferenceFilter {
    fn get_properties(&mut self) -> Properties {
        let mut p = Properties::new();
    
        p.add(
            SETTING_MODEL_PATH, 
            obs_string!("模型路径"), 
            PathProp::new(PathType::File).with_filter(obs_string!("ONNX 模型文件 (*.onnx)"))
        );

        p.add(
            SETTING_INDEX_PATH,
            obs_string!("RVC 音高索引文件路径"), 
            PathProp::new(PathType::File).with_filter(obs_string!("Index 文件 (*.index)"))
        );

        p.add(
            SETTING_DEST_SAMPLE_RATE,
            obs_string!("模型目标采样率"),
            NumberProp::new_int().with_range(16000..=48000).with_step(4000).with_slider()
        );

        p.add(
            SETTING_PITCH_SHIFT, 
            obs_string!("音调设置"),
            NumberProp::new_int().with_range(-24..=24).with_step(1).with_slider()
        );

        p.add(
            SETTING_RESONANCE_SHIFT, 
            obs_string!("共振偏移"),
            NumberProp::new_float(0.07).with_range(-5.0..=5.0).with_slider()
        );

        p.add(
            SETTING_INDEX_RATE, 
            obs_string!("索引率"), 
            NumberProp::new_float(0.01).with_range(0.00..=1.00).with_slider()
        );

        p.add(
            SETTING_LOUDNESS_FACTOR, 
            obs_string!("响度因子"), 
            NumberProp::new_float(0.01).with_range(0.00..=1.00).with_slider()
        );

        p.add(
            SETTING_SAMPLE_LENGTH, 
            obs_string!("采样长度"), 
            NumberProp::new_float(0.01).with_range(0.01..=1.50).with_slider()
        );


        p.add(
            SETTING_FADE_LENGTH, 
            obs_string!("淡入淡出长度"), 
            NumberProp::new_float(0.01).with_range(0.01..=0.15).with_slider()
        );

        p.add(
            SETTING_EXTRA_INFERENCE_TIME, 
            obs_string!("额外推理时长"), 
            NumberProp::new_float(0.01).with_range(0.00..=5.00).with_slider()
        );

        p
    }
}

impl UpdateSource for RvcInferenceFilter {
    fn update(&mut self, settings: &mut DataObj, context: &mut GlobalContext) {
        let mut state = self.shared_state.state.lock();

        let sample_rate = context.with_audio(|audio| audio.sample_rate());
        state.sample_rate = sample_rate;

        let model_changed = get_path_from_settings!(state.model_path, settings, SETTING_MODEL_PATH);
        get_path_from_settings!(state.index_path, settings, SETTING_INDEX_PATH);

        let mut recalculate_input_buffer = false;

        if let Some(new_pitch_shift) = settings.get(SETTING_PITCH_SHIFT) {
            if state.pitch_shift != new_pitch_shift {
                state.pitch_shift = new_pitch_shift;
            }
        }

        if let Some(new_resonance_shift) = settings.get(SETTING_RESONANCE_SHIFT) {
            if state.resonance_shift != new_resonance_shift {
                state.resonance_shift = new_resonance_shift;
            }
        }

        if let Some(new_index_rate) = settings.get(SETTING_INDEX_RATE) {
            if state.index_rate != new_index_rate {
                state.index_rate = new_index_rate;
            }
        }

        if let Some(new_rms_mix_rate) = settings.get(SETTING_LOUDNESS_FACTOR) {
            if state.rms_mix_rate != new_rms_mix_rate {
                state.rms_mix_rate = new_rms_mix_rate;
            }
        }

        if let Some(new_sample_length) = settings.get(SETTING_SAMPLE_LENGTH) {
            if state.sample_length != new_sample_length {
                state.sample_length = new_sample_length;
                recalculate_input_buffer = true;
            }
        }

        if let Some(new_fade_length) = settings.get(SETTING_FADE_LENGTH) {
            if state.crossfade_length != new_fade_length {
                state.crossfade_length = new_fade_length;
                recalculate_input_buffer = true;
            }
        }

        if let Some(new_extra_inference_time) = settings.get(SETTING_EXTRA_INFERENCE_TIME) {
            if state.extra_inference_time != new_extra_inference_time {
                state.extra_inference_time = new_extra_inference_time;
                recalculate_input_buffer = true;
            }
        }

        if let Some(new_dest_sample_rate) = settings.get(SETTING_DEST_SAMPLE_RATE) {
            if state.model_output_sample_rate != new_dest_sample_rate {
                state.model_output_sample_rate = new_dest_sample_rate;
                recalculate_input_buffer = true;
            }
        }

        if model_changed {
            let model_path = state.model_path.clone();
            match model_path {
                Some(path) => {
                    match state.engine.load_model(&path) {
                        Ok(_) => (),
                        Err(e) => {
                            println!("Error loading model: {:?}", e);
                        }
                    }
                }
                None => {
                    state.engine.unload_model();
                }
            }
        }


        if recalculate_input_buffer {
            self.shared_state.buffer_changed.store(true, std::sync::atomic::Ordering::Relaxed);
            let sample_length = state.sample_length;
            let crossfade_length = state.crossfade_length;
            let extra_inference_time = state.extra_inference_time;
            let model_output_sample_rate = state.model_output_sample_rate;

            // zc is sample per 0.1 sec
            let zc = sample_rate / 100;

            let sample_frame_time = (sample_length * sample_rate as f64 / zc as f64).round() as usize;
            let sample_frame_size = sample_frame_time * zc;
            let sample_frame_16k = sample_frame_time * 160;
            let crossfade_frame_size = (crossfade_length * sample_rate as f64 / zc as f64).round() as usize * zc;
            let sola_buffer_frame_size = usize::min(crossfade_frame_size, 4 * zc);
            let sola_search_frame_size = zc;
            let extra_frame_size = (extra_inference_time * sample_rate as f64 / zc as f64).round() as usize * zc;
            let model_return_length = (sample_frame_size + sola_buffer_frame_size + sola_search_frame_size) / zc;
            let model_return_size = model_return_length * (model_output_sample_rate / 100);


            state.sample_frame_size = sample_frame_size;
            state.sample_frame_16k = sample_frame_16k;
            state.crossfade_frame_size = crossfade_frame_size;
            state.sola_buffer_frame_size = sola_buffer_frame_size;
            state.sola_search_frame_size = sola_search_frame_size;
            state.extra_frame_size = extra_frame_size;
            state.model_return_length = model_return_length;
            state.model_return_size = model_return_size;

            let input_buffer_size = extra_frame_size + crossfade_frame_size + sola_search_frame_size + sample_frame_size;
            state.input_buffer.resize(input_buffer_size, 0_f32);

            let input_buffer_16k_size = 160 * input_buffer_size / zc;
            state.input_buffer_16k.resize(input_buffer_16k_size, 0_f32);

            let mut fade_in_window = ndarray::Array1::linspace(0.0, 1.0, sola_buffer_frame_size);
            fade_in_window.mapv_inplace(|x| f32::sin(x * 0.5 * PI).powi(2));
            let fade_out_window = fade_in_window.mapv(|x| 1.0 - x);

            state.fade_in_window = fade_in_window;
            state.fade_out_window = fade_out_window;

            state.input_buffer.fill(0_f32);
            state.input_buffer_16k.fill(0_f32);
            {
                let mut input = self.shared_state.input.lock();
                input.clear();
            }
            {
                let mut output = self.shared_state.output.lock();
                output.clear();
            }

            // TODO: update resampler
        }
        
    }
}

impl FilterAudioSource for RvcInferenceFilter {
    fn filter_audio(&mut self, audio: &mut audio::AudioDataContext) -> FilterAudioResult {

        self.start_thread();

        let timestamp = audio.timestamp();
        let main_channel = downmix_to_mono(audio, self.shared_state.channels).unwrap();

        let frame_len = main_channel.len();
        {
            let mut input = self.shared_state.input.lock();
            let mut timestamps = self.shared_state.timestamps.lock();
            main_channel.iter().for_each(|sample| input.push_back(*sample));
            timestamps.push_back(timestamp);
        }

        self.shared_state.has_input.notify_one();

        {
            let mut output = self.shared_state.output.lock();
            if output.len() < frame_len {
                return FilterAudioResult::Discarded;
            }

            for (channel_stream, output_stream) in main_channel.iter_mut().zip(output.drain(..frame_len)) {
                *channel_stream = output_stream;
            }

            let mut timestamps = self.shared_state.timestamps.lock();
            if let Some(ts) = timestamps.pop_front() {
                audio.set_timestamp(ts);
            }
        }

        upmix_audio_data_context(audio, self.shared_state.channels).unwrap();

        FilterAudioResult::Modified

    }
}

fn process_one_frame(input_sample: &[f32], state: &mut RvcInferenceState) -> ndarray::Array1<f32> {
    let now = Instant::now();

    // move and append the last n samples
    {
        let input_buffer_retaining = state.input_buffer.len() - state.sample_frame_size;
        state.input_buffer.copy_within(state.sample_frame_size.., 0);
        state.input_buffer[input_buffer_retaining..].copy_from_slice(input_sample);
    }

    // resample and set to 16k

    {
        state.input_buffer_16k.copy_within(state.sample_frame_16k.., 0);

        let input_sample = &[input_sample];

        let output_buffer_cap = state.input_buffer_16k.len() - state.sample_frame_16k;
        let output_sample_buffer = &mut state.input_buffer_16k[output_buffer_cap..];
        let output_sample = &mut [output_sample_buffer];
        let result = state.downsampler.process_into_buffer(input_sample, output_sample, None);
        if let Err(e) = result {
            panic!("Error: {:?}", e);
        }
    }

    let input_buffer_view = ndarray::ArrayView1::from_shape(
        (state.input_buffer.len(),), &state.input_buffer
    ).unwrap();

    let input_buffer_16k_view = ndarray::ArrayView1::from_shape(
        (state.input_buffer_16k.len(),), &state.input_buffer_16k
    ).unwrap();

    // inference
    // let output = state.engine.infer(input_buffer_16k_view).unwrap();
    let output = ndarray::Array1::zeros(state.model_return_size);

    let mut output = {
        let output = output.into_raw_vec();
        let output_sample = &[&output];
        let output_buffer = &mut [&mut state.output_buffer[..]];

        let result = state.upsampler.process_into_buffer(output_sample, output_buffer, None);
        if let Err(e) = result {
            panic!("Error: {:?}", e);
        }
        ndarray::ArrayViewMut1::from_shape(
            (state.output_buffer.len(),), &mut state.output_buffer
        ).unwrap()
    };

    // let output = match self.rms_mix_rate < 1. {
    //     true => 
    //         envelop_mixing(&self.input_buffer[self.extra_frame_size..], output, self.sample_rate, self.rms_mix_rate),
    //     false => output,
    // };

    // sola 
    let sola_offset = get_sola_offset(input_buffer_view, state.sola_buffer.view(), 
        state.sola_buffer_frame_size, state.sola_search_frame_size).unwrap();

    let mut output = output.slice_mut(s![sola_offset..]);

    // TODO: phase vocoder
    {
        // output[..self.sola_buffer_frame_size] *= self.fade_in_window;
        let mut output_sola_buffer_view = output.slice_mut(s![..state.sola_buffer_frame_size]);

        output_sola_buffer_view *= &state.fade_in_window;

        let sola_with_fadeout = state.sola_buffer.clone() * &state.fade_out_window;
        output_sola_buffer_view += &sola_with_fadeout;
    }

    // self.sola_buffer.assign(&output[self.sample_frame_size..(self.sample_frame_size + self.sola_buffer_frame_size)]);
    state.sola_buffer.assign(&output.slice(s![state.sample_frame_size..(state.sample_frame_size + state.sola_buffer_frame_size)]));

    // output.iter().for_each(|sample| self.output.push_back(*sample));
    output.slice(s![..state.sample_frame_size]).into_owned()
}

fn thread_loop(shared_state: Arc<RvcInferenceSharedState>) {
    let mut input_sample: Vec<f32> = {
        let state = shared_state.state.lock();
        Vec::with_capacity(state.sample_frame_size)
    };


    while shared_state.running.load(std::sync::atomic::Ordering::Relaxed) {
        let state = shared_state.state.try_lock();
        if state.is_none() {
            continue;
        }
        let mut state = state.unwrap();
        input_sample.clear();
        let sample_frame_size = state.sample_frame_size;
        {
            let mut input = shared_state.input.lock();
            if input.len() < sample_frame_size {
                shared_state.has_input.wait_while_for(&mut input, 
                    |i| i.len() < sample_frame_size, 
                    Duration::from_millis(500));
                continue;
            }
            input_sample.extend(input.drain(..sample_frame_size));
        }
        let output_frame = process_one_frame(&input_sample, &mut state);
        {
            let mut output = shared_state.output.lock();
            output_frame.iter().for_each(|sample| output.push_back(*sample));
        }
    }
}

impl RvcInferenceFilter {
    fn start_thread(&mut self) {
        if self.thread_handle.is_none() {
            let shared_state = self.shared_state.clone();
            let handle = std::thread::spawn(move || thread_loop(shared_state));
            self.thread_handle.replace(handle);
        }
    }
}

impl Drop for RvcInferenceFilter {
    fn drop(&mut self) {
        self.shared_state.running.store(false, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.thread_handle.take() {
            handle.join().unwrap();
        }
    }
}

impl Module for RvcInferenceModule {
    fn new(context: ModuleRef) -> Self {
        Self { context }
    }
    fn get_ctx(&self) -> &ModuleRef {
        &self.context
    }

    fn load(&mut self, load_context: &mut LoadContext) -> bool {
        let source = load_context
            .create_source_builder::<RvcInferenceFilter>()
            .enable_get_name()
            .enable_update()
            .enable_get_properties()
            .enable_filter_audio()
            .build();

        load_context.register_source(source);

        true
    }

    fn description() -> ObsString {
        obs_string!("A filter that uses Retrieval-based Voice Conversion to change your voice.")
    }
    fn name() -> ObsString {
        obs_string!("Retrieval Voice Conversion")
    }
    fn author() -> ObsString {
        obs_string!("Joe")
    }
}

obs_register_module!(RvcInferenceModule);