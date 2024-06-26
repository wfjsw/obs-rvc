use std::path::PathBuf;

use ort::*;

use rvc_common::enums::PitchAlgorithm;

fn get_onnx_session(cache_path: PathBuf, use_tensorrt: bool, use_cudagraph: bool) -> Result<ort::SessionBuilder, ort::Error> {
    #[cfg(feature = "tensorrt")]
    if use_tensorrt {
        return Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_execution_providers([
                TensorRTExecutionProvider::default()
                    .with_timing_cache(true)
                    .with_engine_cache(true)
                    .with_fp16(true)
                    .with_engine_cache_path(cache_path.to_string_lossy())
                    .build(),
                CUDAExecutionProvider::default()
                    .with_copy_in_default_stream(false)
                    .with_cuda_graph()
                    .build(),
                CPUExecutionProvider::default().build(),
            ]);
    } 

    if use_cudagraph {
        return Session::builder()?
            .with_optimization_level(GraphOptimizationLevel::Level3)?
            .with_execution_providers([
                CUDAExecutionProvider::default()
                    .with_cuda_graph()
                    .build(),
                CPUExecutionProvider::default().build(),
            ]);
    }

    Session::builder()?
    .with_optimization_level(GraphOptimizationLevel::Level3)?
    .with_execution_providers([
        CUDAExecutionProvider::default()
            .build(),
        CPUExecutionProvider::default().build(),
    ])

}

pub fn load_model_from_file(model_path: PathBuf, cache_path: PathBuf) -> Result<Session, ort::Error> {
    get_onnx_session(cache_path, false, false)?.commit_from_file(model_path)
}

pub fn load_contentvec_from_file(
    path: PathBuf,
    cache_path: PathBuf,
    text_encoder_in_channels: usize,
    output_layers: usize,
) -> Result<Session, ort::Error> {
    let filename = format!(
        "vec-{}-layer-{}.onnx",
        text_encoder_in_channels, output_layers
    );
    let model_path = path.join(filename);
    get_onnx_session(cache_path, false, false)?.commit_from_file(model_path)
}

pub fn load_f0_from_file(
    path: PathBuf,
    cache_path: PathBuf,
    pitch_algoritm: PitchAlgorithm,
) -> Result<Session, ort::Error> {
    let filename = match pitch_algoritm {
        PitchAlgorithm::Rmvpe => "rmvpe.onnx",
    };

    get_onnx_session(cache_path, false, false)?.commit_from_file(path.join(filename))
}
