use crate::{
    wasi::{
        self,
        clocks::monotonic_clock::{Duration, Instant, Pollable},
    },
    State,
};

impl wasi::clocks::monotonic_clock::Host for State {
    fn now(&mut self) -> wasmtime::Result<Instant> {
        todo!()
    }

    fn resolution(&mut self) -> wasmtime::Result<Duration> {
        todo!()
    }

    fn subscribe_instant(
        &mut self,
        when: Instant,
    ) -> wasmtime::Result<wasmtime::component::Resource<Pollable>> {
        todo!()
    }

    fn subscribe_duration(
        &mut self,
        when: Duration,
    ) -> wasmtime::Result<wasmtime::component::Resource<Pollable>> {
        todo!()
    }
}
