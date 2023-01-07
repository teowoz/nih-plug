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

struct DelayLine {
    buffer: Vec<f32>,
    read_pos: usize,
    write_pos: usize
}

impl DelayLine {
    fn new(capacity: usize) -> DelayLine {
        DelayLine { buffer: vec![0.0; capacity], read_pos: 0, write_pos: 0 }
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
    
    delay_line: Box<DelayLine>,

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

            // FIXME initializing fake resampler, stupid.
            resampler: libsoxr::Soxr::create(1.0, 2.0, 1, None, None, None).unwrap(),
            current_speed: speed_to_uint(DEFAULT_TAPE_SPEED),
            recorded_speed: speed_to_uint(DEFAULT_TAPE_SPEED),
            current_timestamp: 0,

            delay_line: Box::new(DelayLine::new(0)),
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

        let input_fn = |dl: &mut Box<DelayLine>, whole_buffer: &mut [f32], req_count: usize| {
            let buffer = &mut whole_buffer[..req_count];
            if dl.read_pos < dl.write_pos {
                // a trivial case of contiguous buffer
                let read_end = dl.read_pos + req_count;
                if read_end < dl.write_pos {
                    buffer[..].clone_from_slice(&dl.buffer[dl.read_pos..read_end]);
                    dl.read_pos = read_end;
                } else {
                    println!("delay line starved! needs {} samples, has {}", req_count, dl.write_pos-dl.read_pos);
                }
            } else {
                let avail = dl.buffer.len() - dl.read_pos;
                if avail > req_count {
                    let read_end = dl.read_pos + req_count;
                    buffer[..].clone_from_slice(&dl.buffer[dl.read_pos..read_end]);
                    dl.read_pos = read_end;
                } else {
                    buffer[..avail].clone_from_slice(&dl.buffer[dl.read_pos..]);
                    let remaining = req_count - avail;
                    if remaining < dl.write_pos {
                        buffer[avail..].clone_from_slice(&dl.buffer[..remaining]);
                        dl.read_pos = remaining;
                    } else {
                        println!("delay line fragmented and starved! needs {} samples, has {}", remaining, dl.write_pos);
                        dl.read_pos = 0;
                    }
                }
            }
            return Ok(req_count);
        };
        self.resampler.set_input(input_fn, Some(&mut self.delay_line), MAX_BLOCK_SIZE * MAX_SPEED_FACTOR as usize).unwrap();

        // process(...) needs to process samples in place so we need some space which should never exceed block size:
        self.delay_line = Box::new(DelayLine::new((self.sample_rate * LENGTH_IN_SECONDS) as usize + MAX_BLOCK_SIZE*2));
        self.delay_line.write_pos = (self.sample_rate * LENGTH_IN_SECONDS / DEFAULT_TAPE_SPEED) as usize;
        self.changes.resize(self.delay_line.buffer.len() / MIN_BLOCK_SIZE, SpeedChange::default());
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
        let mut tape_speed = self.params.tape_speed.value();
        if self.params.tape_speed.smoothed.is_smoothing() {
            tape_speed = self.params.tape_speed.smoothed.next();
            self.current_speed = speed_to_uint(tape_speed);
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
            let ratio = (self.current_speed as f64) / (self.recorded_speed as f64);
            self.resampler.set_io_ratio(ratio, buffer.len()).unwrap();

            println!("new ratio: {}; buffered in delay line = {}s pos: write {} read {}, deduced from speed = {}s", ratio, ((self.delay_line.write_pos-self.delay_line.read_pos+self.delay_line.buffer.len())%self.delay_line.buffer.len()) as f32/self.sample_rate, self.delay_line.write_pos, self.delay_line.read_pos, LENGTH_IN_SECONDS/tape_speed);
        }

        let iosamples: &mut [f32] = buffer.as_slice()[0];

        if iosamples.len() > MAX_BLOCK_SIZE || iosamples.len() < MIN_BLOCK_SIZE {
            panic!("block size invalid: {}, should be between {} and {}", buffer.len(), MIN_BLOCK_SIZE, MAX_BLOCK_SIZE);
        }

        self.current_timestamp += self.current_speed * iosamples.len() as u32;

        //if self.delay_line_write_pos==self.delay_line_read_pos { println!("delay line empty/overflow @ before writing"); }

        let end_index = self.delay_line.write_pos + iosamples.len();
        if end_index <= self.delay_line.buffer.len() {
            self.delay_line.buffer[self.delay_line.write_pos..end_index].clone_from_slice(&iosamples);
            self.delay_line.write_pos = end_index % self.delay_line.buffer.len();
        } else {
            let boundary = self.delay_line.buffer.len() - self.delay_line.write_pos;
            self.delay_line.buffer[self.delay_line.write_pos..].clone_from_slice(&iosamples[..boundary]);
            self.delay_line.write_pos = end_index - self.delay_line.buffer.len();
            self.delay_line.buffer[..self.delay_line.write_pos].clone_from_slice(&iosamples[boundary..]);
        }

        //if self.delay_line_write_pos==self.delay_line_read_pos { println!("delay line empty/overflow @ after writing"); }

        let done_out: usize = self.resampler.output(iosamples, iosamples.len());
        if done_out != iosamples.len() {
            println!("resampler didn't produce enough samples, done {}, block size {}", done_out, iosamples.len());
        }

        ProcessStatus::Normal
    }
}


impl Vst3Plugin for VariSpeedDelay {
    const VST3_CLASS_ID: [u8; 16] = *b"VSDelay.LUMIFAZA";
    const VST3_CATEGORIES: &'static str = "Fx|Delay";
}

nih_export_vst3!(VariSpeedDelay);