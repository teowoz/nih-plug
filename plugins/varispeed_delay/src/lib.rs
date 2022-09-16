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
use rubato::{InterpolationParameters, InterpolationType, Resampler, SincFixedIn, SincFixedOut, WindowFunction};

const LENGTH_IN_SAMPLES: usize = 480000;
const MAX_SPEED_FACTOR: f32 = 8.0;

struct VariSpeedDelay {
    params: Arc<VariSpeedDelayParams>,

    sample_rate: f32,
    chunk_size: usize,

    // state here
    resampler_in: SincFixedIn::<f64>,
    resampler_out: SincFixedOut::<f64>,
    resampler_in_output: Vec<Vec<f64>>,
    resampler_out_output: Vec<Vec<f64>>,
    resampler_out_input: Vec<Vec<f64>>,
    
    delay_line: Vec<[f64; LENGTH_IN_SAMPLES]>,
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
                1.0,
                FloatRange::Linear {
                    min: 1.0/MAX_SPEED_FACTOR,
                    max: MAX_SPEED_FACTOR,
                },
            )
            .with_smoother(SmoothingStyle::Logarithmic(10.0))
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

    const DEFAULT_INPUT_CHANNELS: u32 = 2;
    const DEFAULT_OUTPUT_CHANNELS: u32 = 2;

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
        _context: &mut impl InitContext,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;

        let sinc_len = 256;
        let f_cutoff = 0.9473371669037001;
        let params = InterpolationParameters {
            sinc_len,
            f_cutoff,
            interpolation: InterpolationType::Cubic,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        // we don't want SR < native SR so make 1.0 the smallest possible ratio:
        // FIXME: according to rubato docs aliasing may happen
        let initial_resample_ratio = MAX_SPEED_FACTOR;
        let max_relative_ratio = MAX_SPEED_FACTOR;
        let channels = buffer_config.num_output_channels;
        self.resampler_in = SincFixedIn::<f64>::new(initial_resample_ratio, max_relative_ratio, params, self.chunk_size, channels).unwrap();
        self.resampler_in_output = self.resampler_in.output_buffer_allocate();
        self.resampler_out = SincFixedOut::<f64>::new(1.0/initial_resample_ratio, max_relative_ratio, params, self.chunk_size, channels).unwrap();
        self.resampler_out_output = self.resampler_out.output_buffer_allocate();
        for i in 0..channels {
            self.resampler_out_input.push(vec![0; self.chunk_size]);
        }
        self.delay_line.resize(channels, [0; LENGTH_IN_SAMPLES]);

        true
    }

    fn reset(&mut self) {
        // TODO

    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        _context: &mut impl ProcessContext,
    ) -> ProcessStatus {
        if self.params.tape_speed.smoothed.is_smoothing() {
            let tape_speed = self.params.tape_speed.smoothed.next();
            self.resampler_in.set_resample_ratio_relative(tape_speed).unwrap();
            self.resampler_out.set_resample_ratio_relative(1.0/tape_speed).unwrap();
        }
        assert_eq!(buffer.len() % self.chunk_size, 0);
        for block in buffer.iter_blocks(self.chunk_size) {
            self.resampler_in.process_into_buffer(&block, &mut self.resampler_in_output, None);
            // TODO suboptimal, excessive copying?
            for (delaybuff, mut res_out_in) in zip(&self.delay_line, &self.resampler_out_input) {
                assert_eq!(self.chunk_size, res_out_in.len());
                for i in 0..self.chunk_size {
                    let dlpos = (self.delay_line_pos + i) % LENGTH_IN_SAMPLES;
                    res_out_in[i] = delaybuff[dlpos];
                }
            }
            for (mut delaybuff, res_in_out) in zip(&self.delay_line, &self.resampler_in_output) {
                assert_eq!(self.chunk_size, res_in_out.len());
                for i in 0..self.chunk_size {
                    let dlpos = (self.delay_line_pos + i) % LENGTH_IN_SAMPLES;
                    delaybuff[dlpos] = res_in_out[i];
                }
            }
            self.delay_line_pos += self.chunk_size;
            self.delay_line_pos %= LENGTH_IN_SAMPLES;
            self.resampler_out.process_into_buffer(&self.resampler_out_input, &mut self.resampler_out_output, None);
    
            for (mut out_samples, res_out) in zip(&block, &self.resampler_out_output) {
                assert_eq!(self.chunk_size, res_out.len());
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
