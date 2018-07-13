use simplelog::Level::Info;

use std::thread;

use ndarray::Axis;
use { Parameters, ProfilingParameters };
use errors::*;
use utils::random_tensor;
use profile::ProfileData;
use rusage::{ Duration, Instant };
use format::*;
use tfdeploy::StreamingInput;
use tfdeploy::StreamingState;
use tfdeploy::Tensor;

fn build_streaming_state(params: &Parameters) -> Result<StreamingState> {
    let input = params.input.as_ref().ok_or("Exactly one of <size> or <data> must be specified.")?;
    let inputs = params.input_node_ids.iter()
        .map(|&s| (s, StreamingInput::Streamed(input.datatype, input.shape.clone())))
        .collect::<Vec<_>>();

    let state = StreamingState::start(
        params.tfd_model.clone(),
        inputs.clone(),
        Some(params.output_node_id)
    )?;
    Ok(state)
}

pub fn handle_cruise(params: Parameters, _profiling: ProfilingParameters) -> Result<()> {
    let start = Instant::now();
    info!("Initializing the StreamingState.");
    let mut state = build_streaming_state(&params)?;
    let measure = Duration::since(&start, 1);
    info!("Initialized the StreamingState in: {}", dur_avg_oneline(measure));

    let input = params.input.ok_or("Exactly one of <size> or <data> must be specified.")?;
    let chunk_shape = input.shape.iter().map(|d| d.unwrap_or(1)).collect::<Vec<_>>();
    let chunk = random_tensor(chunk_shape, input.datatype);

    let buffering = Instant::now();
    info!("Buffering...");
    let mut buffered = 0;
    loop {
        let result = state.step(0, chunk.clone())?;
        if result.len() != 0 {
            break
        }
        buffered += 1;
    }
    info!("Buffered {} chunks in {}", buffered, dur_avg_oneline(Duration::since(&buffering, 1)));
    let mut profile = ProfileData::new(&state.model());
    for _ in 0..100 {
        let _result = state.step_wrapping_ops(params.input_node_ids[0], chunk.clone(),
                    |node, input, buffer| {
                        let start = Instant::now();
                        let r = node.op.step(input, buffer)?;
                        profile.add(node, Duration::since(&start, 1))?;
                        Ok(r)
                    });
    }

    profile.print_most_consuming_nodes(&state.model(), &params.graph, None)?;
    println!();

    profile.print_most_consuming_ops(&state.model())?;

    Ok(())
}

/// Handles the `profile` subcommand when there are streaming dimensions.
pub fn handle_buffering(params: Parameters) -> Result<()> {
    let start = Instant::now();
    info!("Initializing the StreamingState.");
    let state = build_streaming_state(&params)?;
    let measure = Duration::since(&start, 1);
    info!("Initialized the StreamingState in: {}", dur_avg_oneline(measure));

    let mut input = params.input.ok_or("Exactly one of <size> or <data> must be specified.")?;
    let axis = input.shape.iter().position(|&d| d == None).unwrap(); // checked above

    let mut states = (0..100).map(|_| state.clone()).collect::<Vec<_>>();

    if log_enabled!(Info) {
        println!();
        print_header(format!("Streaming profiling for {}:", params.name), "white");
    }

    let shape = input.shape.iter()
        .map(|d| d.unwrap_or(20))
        .collect::<Vec<_>>();
    let data = input.data.take()
        .unwrap_or_else(|| random_tensor(shape, input.datatype));

    // Split the input data into chunks along the streaming axis.
    macro_rules! split_inner {
        ($constr:path, $array:expr) => ({
            $array.axis_iter(Axis(axis))
                .map(|v| $constr(v.insert_axis(Axis(axis)).to_owned()))
                .collect::<Vec<_>>()
        })
    }

    let chunks = match data {
        Tensor::F64(m) => split_inner!(Tensor::F64, m),
        Tensor::F32(m) => split_inner!(Tensor::F32, m),
        Tensor::I32(m) => split_inner!(Tensor::I32, m),
        Tensor::I8(m) => split_inner!(Tensor::I8, m),
        Tensor::U8(m) => split_inner!(Tensor::U8, m),
        Tensor::String(m) => split_inner!(Tensor::String, m),
    };

    let mut profile = ProfileData::new(&state.model());

    for (step, chunk) in chunks.into_iter().enumerate() {
        for &input in &params.input_node_ids {
            trace!("Starting step {:?} with input {:?}.", step, chunk);

            let mut input_chunks = vec![Some(chunk.clone()); 100];
            let mut outputs = Vec::with_capacity(100);
            let start = Instant::now();

            for i in 0..100 {
                outputs.push(states[i].step_wrapping_ops(input, input_chunks[i].take().unwrap(),
                    |node, input, buffer| {
                        let start = Instant::now();
                        let r = node.op.step(input, buffer)?;
                        profile.add(node, Duration::since(&start, 1))?;
                        Ok(r)
                    }
                ));
            }

            let measure = Duration::since(&start, 100);
            println!("Completed step {:2} with output {:?} in: {}", step, outputs[0], dur_avg_oneline(measure));
            thread::sleep(::std::time::Duration::from_secs(1));
        }
    }

    println!();
    print_header(format!("Summary for {}:", params.name), "white");

    profile.print_most_consuming_nodes(&state.model(), &params.graph, None)?;
    println!();

    profile.print_most_consuming_ops(&state.model())?;

    Ok(())
}

