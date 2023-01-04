// Loudness War Winner: Because negative LUFS are boring
// Copyright (C) 2022 Robbert van der Helm
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.

use nih_plug::prelude::*;
use std::sync::Arc;
use std::iter::zip;
use libsoxr;

const LENGTH_IN_SAMPLES: usize = 480000;
const MAX_SPEED_FACTOR: f32 = 640.0;
const DEFAULT_TAPE_SPEED: f32 = 40.0;
const SOXR_DATA_TYPE: libsoxr::Datatype = libsoxr::Datatype::Float32I;

struct VariSpeedDelay {
    params: Arc<VariSpeedDelayParams>,

    sample_rate: f32,
    chunk_size: usize,

    // state here
    resampler_in: libsoxr::Soxr,
    resampler_out: libsoxr::Soxr,
    resampler_in_output: Vec<Vec<f32>>,
    resampler_out_output: Vec<Vec<f32>>,
    resampler_out_input: Vec<Vec<f32>>,
    
    delay_line: Vec<[f32; LENGTH_IN_SAMPLES]>,
    delay_line_pos: usize,
}

#[derive(Params)]
struct VariSpeedDelayParams {
    /// Tape speed
    #[id = "tape_speed"]
    tape_speed: FloatParam,

}

impl Default for VariSpeedDelay {
    fn default() -> Self {
        Self {
            params: Arc::new(VariSpeedDelayParams::default()),

            sample_rate: 1.0,
            chunk_size: 64,

            // FIXME initializing fake resamplers, stupid.
            resampler_in: libsoxr::Soxr::create(1.0, 2.0, 1, None, None, None).unwrap(),
            resampler_out: libsoxr::Soxr::create(2.0, 1.0, 1, None, None, None).unwrap(),

            resampler_in_output: Vec::new(),
            resampler_out_output: Vec::new(),
            resampler_out_input: Vec::new(),
            delay_line: Vec::new(),
            delay_line_pos: 0,
        }
    }
}

impl Default for VariSpeedDelayParams {
    fn default() -> Self {
        Self {
            tape_speed: FloatParam::new(
                "Tape speed",
                DEFAULT_TAPE_SPEED,
                FloatRange::Skewed {
                    min: 1.0,
                    max: MAX_SPEED_FACTOR,
                    factor: 0.33
                },
            )
            .with_smoother(SmoothingStyle::Linear(0.05))
            .with_unit(" ips"),
        }
    }
}

impl Plugin for VariSpeedDelay {
    const NAME: &'static str = "VariSpeed Delay";
    const VENDOR: &'static str = "Teodor Woźniak";
    const URL: &'static str = "https://github.com/teowoz/nih-plug";
    const EMAIL: &'static str = "twozniak@1tbps.org";

    const VERSION: &'static str = "0.1.0";

    const DEFAULT_INPUT_CHANNELS: u32 = 1;
    const DEFAULT_OUTPUT_CHANNELS: u32 = 1; // TODO: IMPLEMENT FOR MORE

    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn accepts_bus_config(&self, config: &BusConfig) -> bool {
        config.num_input_channels == config.num_output_channels && config.num_input_channels > 0
    }

    fn initialize(
        &mut self,
        bus_config: &BusConfig,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;

        let channels: u32 = bus_config.num_output_channels;
        println!("creating resampler_in {}, {}", self.chunk_size, channels);

        let io_spec = libsoxr::IOSpec::new(SOXR_DATA_TYPE, SOXR_DATA_TYPE);
        let quality = libsoxr::QualitySpec::new(&libsoxr::QualityRecipe::High, libsoxr::QualityFlags::VR);

        // libsoxr in Variable Rate mode needs maximum Input/Output ratio when creating the resampler, provide it:
        // here input SR=host SR is constant so we need minimum Output SR relative to host SR,
        // which is... 1 because we never downsample
        self.resampler_in = libsoxr::Soxr::create(1.0, 1.0, channels, Some(&io_spec), Some(&quality), None).unwrap();

        let internal_chunk_length = self.chunk_size * MAX_SPEED_FACTOR as usize * MAX_SPEED_FACTOR as usize + self.chunk_size*2 + 2;
        self.resampler_in_output.clear();
        for _ in 0..channels {
            self.resampler_in_output.push(vec![0.0; internal_chunk_length]);
        }
        println!("resampler_in_output has {} channels", self.resampler_in_output.len());
        if self.resampler_in_output.len()>0 {
            println!("of {} samples", self.resampler_in_output[0].len());
        }

        // and here output SR = host SR = constant so we need maximum input SR relative to host SR,
        self.resampler_out = libsoxr::Soxr::create(MAX_SPEED_FACTOR as f64, 1.0, channels, Some(&io_spec), Some(&quality), None).unwrap();
        self.resampler_out_output.clear();
        self.resampler_out_input.clear();
        for _ in 0..channels {
            self.resampler_out_output.push(vec![0.0; self.chunk_size]);
            self.resampler_out_input.push(vec![0.0; internal_chunk_length]);
        }
        self.delay_line.resize(channels.try_into().unwrap(), [0.0; LENGTH_IN_SAMPLES]);

        self.resampler_in.set_io_ratio(1.0 / (DEFAULT_TAPE_SPEED as f64), 0).unwrap();
        self.resampler_out.set_io_ratio(DEFAULT_TAPE_SPEED as f64, 0).unwrap();
        true
    }

    fn reset(&mut self) {
        // TODO

    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        if self.params.tape_speed.smoothed.is_smoothing() {
            let tape_speed: f64 = self.params.tape_speed.smoothed.next().into();
            self.resampler_in.set_io_ratio(1.0 / tape_speed, (buffer.len() as f64 * tape_speed) as usize).unwrap();
            self.resampler_out.set_io_ratio(tape_speed, buffer.len()).unwrap();
        }
        assert_eq!(buffer.len() % self.chunk_size, 0);
        for (_, block) in buffer.iter_blocks(self.chunk_size) {
            let mut channels: Vec<&mut[f32]> = Vec::new(); // FIXME not real-time-safe
            for ch in block.into_iter() {
                channels.push(ch);
            }
            let (done_in, internal_len) = self.resampler_in.process(Some(&channels[0]), &mut self.resampler_in_output[0]).unwrap();
            if done_in!=self.chunk_size {
                println!("resampler_in didn't consume all samples, done {}, chunk size {}", done_in, self.chunk_size);
            }
            for ch in &mut self.resampler_out_input { // FIXME not real-time-safe
                ch.resize(internal_len, 0.0);
            }
            // TODO suboptimal, excessive copying? try to use slices as Soxr inputs
            for (delaybuff, res_out_in) in zip(&self.delay_line, &mut self.resampler_out_input) {
                for i in 0..internal_len {
                    let dlpos = (self.delay_line_pos + i) % LENGTH_IN_SAMPLES;
                    res_out_in[i] = delaybuff[dlpos];
                }
            }
            for (delaybuff, res_in_out) in zip(&mut self.delay_line, &self.resampler_in_output) {
                for i in 0..internal_len {
                    let dlpos = (self.delay_line_pos + i) % LENGTH_IN_SAMPLES;
                    delaybuff[dlpos] = res_in_out[i];
                }
            }
            self.delay_line_pos += internal_len;
            self.delay_line_pos %= LENGTH_IN_SAMPLES;
            let (done_internal, done_out) = self.resampler_out.process(Some(&self.resampler_out_input[0]), &mut self.resampler_out_output[0]).unwrap();
            if done_out!=self.chunk_size {
                println!("resampler_out didn't produce enough samples, done {}, chunk size {}", done_out, self.chunk_size);
            }
            if done_internal!=internal_len {
                println!("produced and consumed different number of samples! {} != {}", internal_len, done_internal);
            }
    
            for (out_samples, res_out) in zip(&mut channels, &self.resampler_out_output) {
                assert_eq!(self.chunk_size, res_out.len()); // XXX
                for i in 0..self.chunk_size {
                    out_samples[i] = res_out[i];
                }
            }

        }

        ProcessStatus::Normal
    }
}


impl Vst3Plugin for VariSpeedDelay {
    const VST3_CLASS_ID: [u8; 16] = *b"VSDelay.LUMIFAZA";
    const VST3_CATEGORIES: &'static str = "Fx|Delay";
}

nih_export_vst3!(VariSpeedDelay);
