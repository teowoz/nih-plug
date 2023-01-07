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
use libsoxr;

const LENGTH_IN_SECONDS: f32 = 10.0;
const MAX_SPEED_FACTOR: f32 = 640.0;
const DEFAULT_TAPE_SPEED: f32 = 40.0;
const MIN_BLOCK_SIZE: usize = 16;
const MAX_BLOCK_SIZE: usize = 16384;
const SOXR_DATA_TYPE: libsoxr::Datatype = libsoxr::Datatype::Float32I;

#[derive(Clone)]
struct SpeedChange {
    timestamp: u32,
    speed: u32
}

impl Default for SpeedChange {
    fn default() -> Self {
        Self {
            timestamp: 0,
            speed: 0
        }
    }
}

struct VariSpeedDelay {
    params: Arc<VariSpeedDelayParams>,

    sample_rate: f32,

    // state here
    resampler: libsoxr::Soxr,
    current_speed: u32,
    recorded_speed: u32,
    current_timestamp: u32,
    
    delay_line: Vec<f32>,
    delay_line_read_pos: usize,
    delay_line_write_pos: usize,

    changes: Vec<SpeedChange>,
    changes_read_pos: usize,
    changes_write_pos: usize
}

#[derive(Params)]
struct VariSpeedDelayParams {
    /// Tape speed
    #[id = "tape_speed"]
    tape_speed: FloatParam,

}

fn speed_to_uint(speed: f32) -> u32 {
    (speed * 256.0 + 0.5) as u32
}

impl Default for VariSpeedDelay {
    fn default() -> Self {
        Self {
            params: Arc::new(VariSpeedDelayParams::default()),

            sample_rate: 1.0,

            // FIXME initializing fake resamplers, stupid.
            resampler: libsoxr::Soxr::create(1.0, 2.0, 1, None, None, None).unwrap(),
            current_speed: speed_to_uint(DEFAULT_TAPE_SPEED),
            recorded_speed: speed_to_uint(DEFAULT_TAPE_SPEED),
            current_timestamp: 0,

            delay_line: Vec::new(),
            delay_line_read_pos: 0,
            delay_line_write_pos: 0,
            changes: Vec::new(),
            changes_read_pos: 0,
            changes_write_pos: 0
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
    const DEFAULT_OUTPUT_CHANNELS: u32 = 1;

    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn accepts_bus_config(&self, config: &BusConfig) -> bool {
        config.num_input_channels == 1 && config.num_output_channels == 1
    }

    fn initialize(
        &mut self,
        bus_config: &BusConfig,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;

        let channels: u32 = bus_config.num_output_channels;
        let io_spec = libsoxr::IOSpec::new(SOXR_DATA_TYPE, SOXR_DATA_TYPE);
        let quality = libsoxr::QualitySpec::new(&libsoxr::QualityRecipe::High, libsoxr::QualityFlags::VR);

        // libsoxr in Variable Rate mode needs maximum Input/Output ratio when creating the resampler, provide it:
        self.resampler = libsoxr::Soxr::create(MAX_SPEED_FACTOR as f64, 1.0, channels, Some(&io_spec), Some(&quality), None).unwrap();
        // process(...) needs to process samples in place so we need some space which should never exceed block size:
        self.delay_line.resize((self.sample_rate * LENGTH_IN_SECONDS) as usize + MAX_BLOCK_SIZE, 0.0);
        self.delay_line_write_pos = (self.sample_rate * LENGTH_IN_SECONDS) as usize;
        self.changes.resize(self.delay_line.len() / MIN_BLOCK_SIZE, SpeedChange::default());
        self.resampler.set_io_ratio(1.0, 0).unwrap();
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
        let mut change_ratio = false;
        if self.params.tape_speed.smoothed.is_smoothing() {
            self.current_speed = speed_to_uint(self.params.tape_speed.smoothed.next());
            self.changes[self.changes_write_pos] = SpeedChange {
                timestamp: self.current_timestamp + speed_to_uint(LENGTH_IN_SECONDS * self.sample_rate),
                speed: self.current_speed
            };
            println!("added speed change @ {} (now+{}) : current speed = {}", self.changes[self.changes_write_pos].timestamp, self.changes[self.changes_write_pos].timestamp-self.current_timestamp, self.changes[self.changes_write_pos].speed);
            self.changes_write_pos += 1;
            self.changes_write_pos %= self.changes.len();
            if self.changes_read_pos == self.changes_write_pos {
                panic!("changes buffer overflow!");
            }
            change_ratio = true;
        }
        // TODO: we're checking and setting ratio for the whole output block
        // this will be inaccurate in case of fast tape speed changes saved in changes queue
        if self.changes_write_pos != self.changes_read_pos {
            let change = &self.changes[self.changes_read_pos];
            let tsdiff: i32 = (self.current_timestamp - change.timestamp) as i32;
            if tsdiff >= 0 {
                self.recorded_speed = change.speed;
                println!("read speed change @ {} : recorded speed {}; current speed {}", change.timestamp, change.speed, self.current_speed);
                change_ratio = true;
                self.changes_read_pos += 1;
                self.changes_read_pos %= self.changes.len();
            }
        }
        if change_ratio {
            self.resampler.set_io_ratio((self.current_speed as f64) / (self.recorded_speed as f64), buffer.len()).unwrap();
        }

        let iosamples: &mut [f32] = buffer.as_slice()[0];

        if iosamples.len() > MAX_BLOCK_SIZE || iosamples.len() < MIN_BLOCK_SIZE {
            panic!("block size invalid: {}, should be between {} and {}", buffer.len(), MIN_BLOCK_SIZE, MAX_BLOCK_SIZE);
        }

        self.current_timestamp += self.current_speed * iosamples.len() as u32;

        let end_index = self.delay_line_write_pos + iosamples.len();
        if end_index <= self.delay_line.len() {
            self.delay_line[self.delay_line_write_pos..end_index].clone_from_slice(&iosamples);
            self.delay_line_write_pos = end_index % self.delay_line.len();
        } else {
            let boundary = self.delay_line.len() - self.delay_line_write_pos;
            self.delay_line[self.delay_line_write_pos..].clone_from_slice(&iosamples[..boundary]);
            self.delay_line_write_pos = end_index - self.delay_line.len();
            self.delay_line[..self.delay_line_write_pos].clone_from_slice(&iosamples[boundary..]);
        }

        if self.delay_line_read_pos < self.delay_line_write_pos {
            // contiguous buffer
            let in0 = &self.delay_line[self.delay_line_read_pos..self.delay_line_write_pos];
            let (done_in, done_out) = self.resampler.process(Some(in0), iosamples).unwrap();
            if done_out != iosamples.len() {
                println!("resampler didn't produce enough samples, done {}, block size {}", done_out, iosamples.len());
            }
            self.delay_line_read_pos += done_in;
            //self.delay_line_read_pos %= self.delay_line.len();
            if !(self.delay_line_read_pos < self.delay_line.len()) {
                println!("read pos {}, delay len {}, write pos {}", self.delay_line_read_pos, self.delay_line.len(), self.delay_line_write_pos);
            }
            assert!(self.delay_line_read_pos < self.delay_line.len());
        } else {
            let in1 = &self.delay_line[self.delay_line_read_pos..];
            let (done_in1, done_out1) = self.resampler.process(Some(in1), iosamples).unwrap();
            if done_out1 != iosamples.len() {
                // need more data from delay line
                let in2 = &self.delay_line[..self.delay_line_write_pos];
                let (done_in2, done_out2) = self.resampler.process(Some(in2), &mut iosamples[done_out1..]).unwrap();
                if done_out1 + done_out2 != iosamples.len() {
                    println!("resampler didn't produce enough samples, done {}+{}, block size {}", done_out1, done_out2, iosamples.len());
                }
                self.delay_line_read_pos = done_in2;
            } else {
                self.delay_line_read_pos += done_in1;
                self.delay_line_read_pos %= self.delay_line.len();
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
