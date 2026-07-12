use crate::message::HwMidiEvent;

pub trait HwWorkerDriver {
    fn cycle_samples(&self) -> usize;
    fn sample_rate(&self) -> i32;
    fn request_stop(&mut self) {}
    fn close_fds(&mut self) {}
    fn set_playing(&mut self, _playing: bool) {}
    fn set_output_gain_balance(&mut self, _gain: f32, _balance: f32) {}
    fn run_cycle_for_worker(&mut self) -> Result<(), String>;
    fn run_assist_step_for_worker(&mut self) -> Result<bool, String>;

    /// Give the driver the render-plan slot so its RT cycle can read/write
    /// plan arena buffers instead of the legacy port buffers. Drivers that
    /// do not support plan-based I/O yet ignore it.
    fn set_plan_slot(&mut self, _slot: std::sync::Arc<crate::render_plan::PlanSlot>) {}

    /// File descriptors for async I/O via kqueue/AsyncFd.
    /// When both return `Some`, the HW worker uses an async select loop
    /// instead of the blocking assist thread.
    #[cfg(unix)]
    fn capture_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }
    #[cfg(unix)]
    fn playback_fd(&self) -> Option<std::os::fd::RawFd> {
        None
    }
}

pub trait HwMidiHub {
    fn open_input(&mut self, _device: &str) -> Result<(), String> {
        Err("Hardware MIDI input devices are not supported by this backend".to_string())
    }
    fn open_output(&mut self, _device: &str) -> Result<(), String> {
        Err("Hardware MIDI output devices are not supported by this backend".to_string())
    }
    fn close_all(&mut self) {}
    fn read_events_into(&mut self, out: &mut Vec<HwMidiEvent>);
    fn read_events_blocking_into(&mut self, out: &mut Vec<HwMidiEvent>) {
        self.read_events_into(out);
    }
    fn wait_ready_blocking(&mut self) -> Option<Vec<i32>> {
        None
    }
    fn read_events_for_fds(&mut self, _ready_fds: &[i32], out: &mut Vec<HwMidiEvent>) {
        self.read_events_into(out);
    }
    fn wake_input_waiter(&mut self) {}
    fn close_input_waiter(&mut self) {}
    fn write_events(&mut self, events: &[HwMidiEvent]);
}

pub trait HwDevice {
    fn input_channels(&self) -> usize;
    fn output_channels(&self) -> usize;
    fn sample_rate(&self) -> i32;
    fn latency_ranges(&self) -> ((usize, usize), (usize, usize));
}

#[macro_export]
macro_rules! impl_hw_worker_traits_for_driver {
    ($driver:ty) => {
        impl $crate::hw::traits::HwWorkerDriver for $driver {
            fn cycle_samples(&self) -> usize {
                self.cycle_samples()
            }

            fn sample_rate(&self) -> i32 {
                self.sample_rate()
            }

            fn close_fds(&mut self) {
                self.close_fds()
            }

            fn set_playing(&mut self, playing: bool) {
                self.set_playing(playing)
            }

            fn set_output_gain_balance(&mut self, gain: f32, balance: f32) {
                self.set_output_gain_balance(gain, balance)
            }

            fn run_cycle_for_worker(&mut self) -> Result<(), String> {
                self.channel().run_cycle().map_err(|e| e.to_string())
            }

            fn run_assist_step_for_worker(&mut self) -> Result<bool, String> {
                self.channel().run_assist_step().map_err(|e| e.to_string())
            }
        }
    };
}

#[macro_export]
macro_rules! impl_hw_device_for_driver {
    ($driver:ty) => {
        impl $crate::hw::traits::HwDevice for $driver {
            fn input_channels(&self) -> usize {
                self.input_channels()
            }

            fn output_channels(&self) -> usize {
                self.output_channels()
            }

            fn sample_rate(&self) -> i32 {
                self.sample_rate()
            }

            fn latency_ranges(&self) -> ((usize, usize), (usize, usize)) {
                self.latency_ranges()
            }
        }
    };
}

#[macro_export]
macro_rules! impl_hw_midi_hub_traits {
    ($hub:ty) => {
        impl $crate::hw::traits::HwMidiHub for $hub {
            fn open_input(&mut self, device: &str) -> Result<(), String> {
                <$hub>::open_input(self, device)
            }

            fn open_output(&mut self, device: &str) -> Result<(), String> {
                <$hub>::open_output(self, device)
            }

            fn close_all(&mut self) {
                <$hub>::close_all(self);
            }

            fn read_events_into(&mut self, out: &mut Vec<$crate::message::HwMidiEvent>) {
                <$hub>::read_events_into(self, out);
            }

            fn read_events_blocking_into(&mut self, out: &mut Vec<$crate::message::HwMidiEvent>) {
                <$hub>::read_events_blocking_into(self, out);
            }

            fn wait_ready_blocking(&mut self) -> Option<Vec<i32>> {
                <$hub>::wait_ready_blocking(self)
            }

            fn read_events_for_fds(
                &mut self,
                ready_fds: &[i32],
                out: &mut Vec<$crate::message::HwMidiEvent>,
            ) {
                <$hub>::read_events_for_fds(self, ready_fds, out)
            }

            fn wake_input_waiter(&mut self) {
                <$hub>::wake_input_waiter(self);
            }

            fn close_input_waiter(&mut self) {
                <$hub>::close_input_waiter(self);
            }

            fn write_events(&mut self, events: &[$crate::message::HwMidiEvent]) {
                <$hub>::write_events(self, events);
            }
        }
    };
}
