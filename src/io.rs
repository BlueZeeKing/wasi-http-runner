use futures::{future::poll_fn, task::noop_waker_ref};
use hyper::body::Body;
use std::{
    collections::VecDeque,
    io::ErrorKind,
    pin::Pin,
    task::{Context, Poll},
    thread,
};

use wasmtime::component::Resource;

use crate::{
    http::BodyState,
    wasi::{
        self,
        io::{
            poll::Pollable,
            streams::{Error, InputStream, OutputStream, StreamError},
        },
    },
    State,
};

pub trait PollableIndividual {
    fn ready(&mut self, state: &mut State) -> wasmtime::Result<bool>;

    fn block(&mut self, state: &mut State) -> wasmtime::Result<()>;

    fn destroy(&mut self, state: &mut State) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl wasi::io::poll::Host for State {
    fn poll(&mut self, in_: Vec<Resource<Pollable>>) -> wasmtime::Result<Vec<u32>> {
        let mut resources = Vec::new();

        for index in in_.into_iter().map(|val| val.rep()) {
            resources.push((
                index,
                self.pollables
                    .remove(&index)
                    .ok_or_else(|| wasmtime::Error::msg("Could not find pollable"))?,
            ));
        }

        let mut ready = Vec::new();

        loop {
            let mut should_break = false;
            for (index, (_, val)) in resources.iter_mut().enumerate() {
                if val.ready(self)? {
                    should_break = true;
                    ready.push(index as u32);
                }
            }

            if should_break {
                break;
            }
        }

        self.pollables.extend(resources.into_iter());

        Ok(ready)
    }
}

impl wasi::io::poll::HostPollable for State {
    fn ready(&mut self, self_: Resource<Pollable>) -> wasmtime::Result<bool> {
        let mut resourse = self
            .pollables
            .remove(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find pollable"))?;

        let res = resourse.ready(self);

        self.pollables.insert(self_.rep(), resourse);

        res
    }

    fn block(&mut self, self_: Resource<Pollable>) -> wasmtime::Result<()> {
        let mut resourse = self
            .pollables
            .remove(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find pollable"))?;

        let res = resourse.block(self);

        self.pollables.insert(self_.rep(), resourse);

        res
    }

    fn drop(&mut self, rep: Resource<Pollable>) -> wasmtime::Result<()> {
        let mut resourse = self
            .pollables
            .remove(&rep.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find pollable"))?;

        let res = resourse.destroy(self);

        self.pollables.insert(rep.rep(), resourse);

        res
    }
}

impl wasi::io::error::Host for State {}

impl wasi::io::error::HostError for State {
    fn to_debug_string(
        &mut self,
        self_: wasmtime::component::Resource<Error>,
    ) -> wasmtime::Result<String> {
        let resource = self
            .errors
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find error"))?;

        Ok(format!("{:?}", resource))
    }

    fn drop(&mut self, rep: wasmtime::component::Resource<Error>) -> wasmtime::Result<()> {
        self.errors.remove(&rep.rep());

        Ok(())
    }
}

impl wasi::io::streams::Host for State {}

impl State {
    fn handle_hyper_error(&mut self, error: hyper::Error) -> Resource<Error> {
        let id = self.new_id();

        self.errors
            .insert(id, std::io::Error::new(ErrorKind::Other, error));

        Resource::new_own(id)
    }
}

impl wasi::io::streams::HostInputStream for State {
    fn read(
        &mut self,
        self_: wasmtime::component::Resource<InputStream>,
        len: u64,
    ) -> wasmtime::Result<Result<Vec<u8>, StreamError>> {
        let resource = self
            .incoming
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find stream"))?;

        if resource.state == BodyState::Consumed {
            return Ok(Err(StreamError::Closed));
        }

        if let Some(frame) = resource.last_frame.take() {
            let mut frame = match frame {
                Ok(v) => v,
                Err(e) => {
                    return Ok(Err(StreamError::LastOperationFailed(
                        self.handle_hyper_error(e),
                    )))
                }
            };

            if frame.is_trailers() {
                resource.trailers = Some(frame.into_trailers().unwrap());
                return Ok(Err(StreamError::Closed));
            }

            let bytes = frame.data_mut().unwrap();
            let mut new = bytes.split_off(len as usize);

            std::mem::swap(bytes, &mut new);

            resource.last_frame = Some(Ok(frame));

            return Ok(Ok(new.to_vec()));
        }

        let Poll::Ready(res) =
            Pin::new(&mut resource.incoming).poll_frame(&mut Context::from_waker(noop_waker_ref()))
        else {
            return Ok(Ok(Vec::new()));
        };

        if let Some(frame) = res {
            let mut frame = match frame {
                Ok(frame) => frame,
                Err(err) => {
                    return Ok(Err(StreamError::LastOperationFailed(
                        self.handle_hyper_error(err),
                    )))
                }
            };

            if frame.is_data() {
                let bytes = frame.data_mut().unwrap();
                let mut new = bytes.split_off(len as usize);

                std::mem::swap(bytes, &mut new);

                resource.last_frame = Some(Ok(frame));

                return Ok(Ok(new.to_vec()));
            } else {
                let trailers = frame.into_trailers().unwrap();
                resource.trailers = Some(trailers);
                resource.state = BodyState::Trailers;
                Ok(Err(StreamError::Closed))
            }
        } else {
            resource.state = BodyState::Consumed;
            Ok(Err(StreamError::Closed))
        }
    }

    fn blocking_read(
        &mut self,
        self_: wasmtime::component::Resource<InputStream>,
        len: u64,
    ) -> wasmtime::Result<Result<Vec<u8>, StreamError>> {
        let resource = self
            .incoming
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find stream"))?;

        if resource.state == BodyState::Consumed {
            return Ok(Err(StreamError::Closed));
        }

        if let Some(frame) = resource.last_frame.take() {
            let mut frame = match frame {
                Ok(v) => v,
                Err(e) => {
                    return Ok(Err(StreamError::LastOperationFailed(
                        self.handle_hyper_error(e),
                    )))
                }
            };

            if frame.is_trailers() {
                resource.trailers = Some(frame.into_trailers().unwrap());
                return Ok(Err(StreamError::Closed));
            }

            let bytes = frame.data_mut().unwrap();
            let mut new = bytes.split_off(len as usize);

            std::mem::swap(bytes, &mut new);

            resource.last_frame = Some(Ok(frame));

            return Ok(Ok(new.to_vec()));
        }

        let res = futures::executor::block_on(poll_fn(|cx| {
            Pin::new(&mut resource.incoming).poll_frame(cx)
        }));

        if let Some(frame) = res {
            let mut frame = match frame {
                Ok(frame) => frame,
                Err(err) => {
                    return Ok(Err(StreamError::LastOperationFailed(
                        self.handle_hyper_error(err),
                    )))
                }
            };

            if frame.is_data() {
                let bytes = frame.data_mut().unwrap();
                let mut new = bytes.split_off(len as usize);

                std::mem::swap(bytes, &mut new);

                resource.last_frame = Some(Ok(frame));

                return Ok(Ok(new.to_vec()));
            } else {
                let trailers = frame.into_trailers().unwrap();
                resource.trailers = Some(trailers);
                resource.state = BodyState::Trailers;
                Ok(Err(StreamError::Closed))
            }
        } else {
            resource.state = BodyState::Consumed;
            Ok(Err(StreamError::Closed))
        }
    }

    fn skip(
        &mut self,
        self_: wasmtime::component::Resource<InputStream>,
        len: u64,
    ) -> wasmtime::Result<Result<u64, StreamError>> {
        self.read(self_, len)
            .map(|val| val.map(|val| val.len() as u64))
    }

    fn blocking_skip(
        &mut self,
        self_: wasmtime::component::Resource<InputStream>,
        len: u64,
    ) -> wasmtime::Result<Result<u64, StreamError>> {
        self.blocking_read(self_, len)
            .map(|val| val.map(|val| val.len() as u64))
    }

    fn subscribe(
        &mut self,
        self_: wasmtime::component::Resource<InputStream>,
    ) -> wasmtime::Result<wasmtime::component::Resource<Pollable>> {
        let id = self.new_id();

        self.pollables
            .insert(id, Box::new(InputStreamReady { id: self_.rep() }));

        Ok(Resource::new_own(id))
    }

    fn drop(&mut self, rep: wasmtime::component::Resource<InputStream>) -> wasmtime::Result<()> {
        let resource = self
            .incoming
            .get_mut(&rep.rep())
            .ok_or_else(|| wasmtime::Error::msg("Cannot find stream"))?;

        resource.state = BodyState::Trailers;

        Ok(())
    }
}

struct InputStreamReady {
    id: u32,
}

impl PollableIndividual for InputStreamReady {
    fn ready(&mut self, state: &mut State) -> wasmtime::Result<bool> {
        let resource = state
            .incoming
            .get_mut(&self.id)
            .ok_or_else(|| wasmtime::Error::msg("Cannot find stream"))?;

        let Poll::Ready(res) =
            Pin::new(&mut resource.incoming).poll_frame(&mut Context::from_waker(noop_waker_ref()))
        else {
            return Ok(false);
        };

        if let Some(frame) = res {
            resource.last_frame = Some(frame);
        } else {
            resource.state = BodyState::Consumed;
        }

        Ok(true)
    }

    fn block(&mut self, state: &mut State) -> wasmtime::Result<()> {
        let resource = state
            .incoming
            .get_mut(&self.id)
            .ok_or_else(|| wasmtime::Error::msg("Cannot find stream"))?;

        let res = futures::executor::block_on(poll_fn(|cx| {
            Pin::new(&mut resource.incoming).poll_frame(cx)
        }));

        if let Some(frame) = res {
            resource.last_frame = Some(frame);
        } else {
            resource.state = BodyState::Consumed;
        }

        Ok(())
    }
}

const BUF_LIMIT: usize = 4096;

impl wasi::io::streams::HostOutputStream for State {
    fn check_write(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
    ) -> wasmtime::Result<Result<u64, StreamError>> {
        let resource = self
            .responses
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find response body"))?
            .body_mut();

        Ok(Ok((BUF_LIMIT - resource.buf.len()) as u64))
    }

    fn write(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
        contents: Vec<u8>,
    ) -> wasmtime::Result<Result<(), StreamError>> {
        let resource = self
            .responses
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find response body"))?
            .body_mut();

        resource.buf.append(&mut VecDeque::from(contents));
        resource.wake();

        Ok(Ok(()))
    }

    fn blocking_write_and_flush(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
        contents: Vec<u8>,
    ) -> wasmtime::Result<Result<(), StreamError>> {
        let resource = self
            .responses
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find response body"))?
            .body_mut();

        resource.buf.append(&mut VecDeque::from(contents));

        self.blocking_flush(self_)
    }

    fn flush(
        &mut self,
        _self_: wasmtime::component::Resource<OutputStream>,
    ) -> wasmtime::Result<Result<(), StreamError>> {
        Ok(Ok(()))
    }

    fn blocking_flush(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
    ) -> wasmtime::Result<Result<(), StreamError>> {
        let resource = self
            .responses
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find response body"))?
            .body_mut();

        while resource.buf.len() > 0 {
            resource.wake();
            thread::park();
        }

        Ok(Ok(()))
    }

    fn subscribe(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
    ) -> wasmtime::Result<wasmtime::component::Resource<Pollable>> {
        let id = self.new_id();
        self.pollables
            .insert(id, Box::new(OutputPollable { id: self_.rep() }));

        Ok(Resource::new_own(id))
    }

    fn write_zeroes(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
        len: u64,
    ) -> wasmtime::Result<Result<(), StreamError>> {
        self.write(self_, vec![0; len as usize])
    }

    fn blocking_write_zeroes_and_flush(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
        len: u64,
    ) -> wasmtime::Result<Result<(), StreamError>> {
        self.blocking_write_and_flush(self_, vec![0; len as usize])
    }

    fn splice(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
        src: wasmtime::component::Resource<InputStream>,
        len: u64,
    ) -> wasmtime::Result<Result<u64, StreamError>> {
        todo!()
    }

    fn blocking_splice(
        &mut self,
        self_: wasmtime::component::Resource<OutputStream>,
        src: wasmtime::component::Resource<InputStream>,
        len: u64,
    ) -> wasmtime::Result<Result<u64, StreamError>> {
        todo!()
    }

    fn drop(&mut self, rep: wasmtime::component::Resource<OutputStream>) -> wasmtime::Result<()> {
        Ok(())
    }
}

struct OutputPollable {
    id: u32,
}

impl PollableIndividual for OutputPollable {
    fn ready(&mut self, state: &mut State) -> wasmtime::Result<bool> {
        let resource = state
            .responses
            .get(&self.id)
            .ok_or_else(|| wasmtime::Error::msg("Could not find output body"))?;

        Ok(resource.body().buf.len() < BUF_LIMIT)
    }

    fn block(&mut self, state: &mut State) -> wasmtime::Result<()> {
        let resource = state
            .responses
            .get_mut(&self.id)
            .ok_or_else(|| wasmtime::Error::msg("Could not find output body"))?
            .body_mut();

        while resource.buf.len() >= BUF_LIMIT {
            resource.thread = Some(thread::current());
            thread::park();
        }

        Ok(())
    }
}
