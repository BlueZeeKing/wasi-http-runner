use std::{
    collections::VecDeque,
    convert::Infallible,
    pin::Pin,
    task::{Context, Poll, Waker},
    thread::Thread,
};

use crate::{io::PollableIndividual, wasi::http::types::Duration};

use super::wasi::{
    self,
    http::types::{
        ErrorCode, FieldKey, FieldValue, Fields, FutureIncomingResponse, FutureTrailers,
        HeaderError, Headers, IncomingBody, IncomingRequest, IncomingResponse, InputStream,
        IoError, Method, OutgoingBody, OutgoingRequest, OutgoingResponse, OutputStream,
        RequestOptions, ResponseOutparam, Scheme, StatusCode, Trailers,
    },
    io::poll::Pollable,
};
use futures::{future::poll_fn, task::noop_waker_ref};
use http::{header::Entry, HeaderMap, HeaderName, HeaderValue, Response};
use hyper::body::{Body, Bytes, Frame, Incoming};
use wasmtime::component::Resource;

use super::State;

impl wasi::http::types::Host for State {
    fn http_error_code(&mut self, err: Resource<IoError>) -> wasmtime::Result<Option<ErrorCode>> {
        let val = self
            .errors
            .get(&err.rep())
            .ok_or_else(|| wasmtime::Error::msg("Unable to find error resource"))?;

        Ok(Some(ErrorCode::InternalError(Some(format!("{}", val)))))
    }
}

impl wasi::http::types::HostFields for State {
    fn new(&mut self) -> wasmtime::Result<Resource<Fields>> {
        let id = self.new_id();
        self.fields.insert(id, (false, HeaderMap::new()));
        Ok(Resource::new_own(id))
    }

    fn from_list(
        &mut self,
        entries: Vec<(FieldKey, FieldValue)>,
    ) -> wasmtime::Result<Result<Resource<Fields>, HeaderError>> {
        let id = self.new_id();
        self.fields.insert(id, (false, HeaderMap::new()));
        let (_, resource) = self.fields.get_mut(&id).unwrap();

        let headers = entries
            .into_iter()
            .map(|(k, v)| -> Result<(HeaderName, HeaderValue), HeaderError> {
                Ok((
                    HeaderName::try_from(k).map_err(|_| HeaderError::InvalidSyntax)?,
                    HeaderValue::from_bytes(&v).map_err(|_| HeaderError::InvalidSyntax)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>();

        let headers = match headers {
            Ok(val) => val,
            Err(err) => return Ok(Err(err)),
        };

        for (name, value) in headers {
            resource.append(name, value);
        }

        Ok(Ok(Resource::new_own(id)))
    }

    fn get(
        &mut self,
        self_: Resource<Fields>,
        name: FieldKey,
    ) -> wasmtime::Result<Vec<FieldValue>> {
        let val = self
            .fields
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find field"))?
            .1
            .get_all(&match HeaderName::try_from(name) {
                Ok(val) => val,
                Err(_) => return Ok(vec![]),
            });

        Ok(val.iter().map(|val| val.as_bytes().to_vec()).collect())
    }

    fn set(
        &mut self,
        self_: Resource<Fields>,
        name: FieldKey,
        value: Vec<FieldValue>,
    ) -> wasmtime::Result<Result<(), HeaderError>> {
        let (immutable, resourse) = self
            .fields
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find field"))?;

        if *immutable {
            return Ok(Err(HeaderError::Immutable));
        }

        let name = match HeaderName::try_from(name) {
            Ok(val) => val,
            Err(_) => return Ok(Err(HeaderError::InvalidSyntax)),
        };

        let mut vals = value.into_iter().map(|val| HeaderValue::try_from(val));

        if let Some(val) = vals.next() {
            let val = match val {
                Ok(v) => v,
                Err(_) => return Ok(Err(HeaderError::InvalidSyntax)),
            };
            resourse.insert(name.clone(), val);
        } else {
            resourse.remove(name.clone());
        }

        for val in vals {
            let val = match val {
                Ok(v) => v,
                Err(_) => return Ok(Err(HeaderError::InvalidSyntax)),
            };
            resourse.append(name.clone(), val);
        }

        Ok(Ok(()))
    }

    fn delete(
        &mut self,
        self_: Resource<Fields>,
        name: FieldKey,
    ) -> wasmtime::Result<Result<(), HeaderError>> {
        let (immutable, resource) = self
            .fields
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find field"))?;

        if *immutable {
            return Ok(Err(HeaderError::Immutable));
        }

        resource.remove(&match HeaderName::try_from(name) {
            Ok(val) => val,
            Err(_) => return Ok(Err(HeaderError::InvalidSyntax)),
        });

        Ok(Ok(()))
    }

    fn append(
        &mut self,
        self_: Resource<Fields>,
        name: FieldKey,
        value: FieldValue,
    ) -> wasmtime::Result<Result<(), HeaderError>> {
        let (immutable, resource) = self
            .fields
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find field"))?;

        if *immutable {
            return Ok(Err(HeaderError::Immutable));
        }

        let value = match HeaderValue::try_from(value) {
            Ok(val) => val,
            Err(_) => return Ok(Err(HeaderError::InvalidSyntax)),
        };

        match resource.entry(match HeaderName::try_from(name) {
            Ok(val) => val,
            Err(_) => return Ok(Err(HeaderError::InvalidSyntax)),
        }) {
            Entry::Occupied(mut entry) => {
                entry.append(value);
            }
            Entry::Vacant(entry) => {
                entry.insert(value);
            }
        }

        Ok(Ok(()))
    }

    fn entries(
        &mut self,
        self_: Resource<Fields>,
    ) -> wasmtime::Result<Vec<(FieldKey, FieldValue)>> {
        let (_, resource) = self
            .fields
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find field"))?;

        Ok(resource
            .iter()
            .map(|(key, value)| (key.to_string(), value.as_bytes().to_vec()))
            .collect())
    }

    fn clone(&mut self, self_: Resource<Fields>) -> wasmtime::Result<Resource<Fields>> {
        let id = self.new_id();

        let resource = self
            .fields
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find field"))?
            .clone();

        self.fields.insert(id, resource);

        Ok(Resource::new_own(id))
    }

    fn drop(&mut self, rep: Resource<Fields>) -> wasmtime::Result<()> {
        self.fields.remove(&rep.rep());

        Ok(())
    }
}

impl wasi::http::types::HostIncomingRequest for State {
    fn method(&mut self, self_: Resource<IncomingRequest>) -> wasmtime::Result<Method> {
        let resource = self
            .requests
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find request"))?;

        let method = resource.method();

        if method == http::Method::GET {
            Ok(Method::Get)
        } else if method == http::Method::HEAD {
            Ok(Method::Head)
        } else if method == http::Method::POST {
            Ok(Method::Post)
        } else if method == http::Method::PUT {
            Ok(Method::Put)
        } else if method == http::Method::DELETE {
            Ok(Method::Delete)
        } else if method == http::Method::CONNECT {
            Ok(Method::Connect)
        } else if method == http::Method::OPTIONS {
            Ok(Method::Options)
        } else if method == http::Method::TRACE {
            Ok(Method::Trace)
        } else if method == http::Method::PATCH {
            Ok(Method::Patch)
        } else {
            Ok(Method::Other(method.to_string()))
        }
    }

    fn path_with_query(
        &mut self,
        self_: Resource<IncomingRequest>,
    ) -> wasmtime::Result<Option<String>> {
        let resource = self
            .requests
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find request"))?;

        Ok(resource.uri().path_and_query().map(|val| val.to_string()))
    }

    fn scheme(&mut self, self_: Resource<IncomingRequest>) -> wasmtime::Result<Option<Scheme>> {
        let resource = self
            .requests
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find request"))?;

        Ok(resource.uri().scheme().map(|val| {
            if val == &http::uri::Scheme::HTTP {
                Scheme::Http
            } else if val == &http::uri::Scheme::HTTPS {
                Scheme::Https
            } else {
                Scheme::Other(val.to_string())
            }
        }))
    }

    fn authority(&mut self, self_: Resource<IncomingRequest>) -> wasmtime::Result<Option<String>> {
        let resource = self
            .requests
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find request"))?;

        Ok(resource.uri().authority().map(|val| val.to_string()))
    }

    fn headers(&mut self, self_: Resource<IncomingRequest>) -> wasmtime::Result<Resource<Headers>> {
        let id = self.new_id();
        let resource = self
            .requests
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find request"))?;

        self.fields.insert(
            id,
            (
                true,
                HeaderMap::from_iter(
                    resource
                        .headers()
                        .iter()
                        .map(|(key, val)| (key.to_owned(), val.to_owned())),
                ),
            ),
        );

        Ok(Resource::new_own(id))
    }

    fn consume(
        &mut self,
        self_: Resource<IncomingRequest>,
    ) -> wasmtime::Result<Result<Resource<IncomingBody>, ()>> {
        let resource = match self.requests.remove(&self_.rep()) {
            Some(val) => val,
            None => {
                if self.incoming.contains_key(&self_.rep()) {
                    return Ok(Err(()));
                } else {
                    return Err(wasmtime::Error::msg("Could not find resource"));
                }
            }
        };

        self.incoming.insert(
            self_.rep(),
            IncomingBodyWrapper {
                incoming: resource.into_body(),
                state: BodyState::New,
                trailers: None,
                last_frame: None,
            },
        );

        Ok(Ok(Resource::new_own(self_.rep())))
    }

    fn drop(&mut self, rep: Resource<IncomingRequest>) -> wasmtime::Result<()> {
        self.requests.remove(&rep.rep());

        Ok(())
    }
}

pub struct IncomingBodyWrapper {
    pub incoming: Incoming,
    pub state: BodyState,
    pub trailers: Option<HeaderMap>,
    pub last_frame: Option<Result<Frame<Bytes>, hyper::Error>>,
}

#[derive(PartialEq)]
pub enum BodyState {
    New,
    Data,
    Trailers,
    Consumed,
}

impl wasi::http::types::HostIncomingBody for State {
    fn stream(
        &mut self,
        self_: Resource<IncomingBody>,
    ) -> wasmtime::Result<Result<Resource<InputStream>, ()>> {
        let resource = self
            .incoming
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find body"))?;

        if resource.state == BodyState::New {
            resource.state = BodyState::Data;

            Ok(Ok(Resource::new_own(self_.rep())))
        } else {
            Ok(Err(()))
        }
    }

    fn finish(
        &mut self,
        this: Resource<IncomingBody>,
    ) -> wasmtime::Result<Resource<FutureTrailers>> {
        let resource = self
            .incoming
            .get_mut(&this.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find body"))?;

        if resource.state != BodyState::Trailers {
            return Err(wasmtime::Error::msg("The body is not ready for trailers"));
        }

        Ok(Resource::new_own(this.rep()))
    }

    fn drop(&mut self, rep: Resource<IncomingBody>) -> wasmtime::Result<()> {
        self.incoming.remove(&rep.rep());

        Ok(())
    }
}

pub struct Outgoing {
    pub buf: VecDeque<u8>, // TODO: maybe use arrays?
    pub waker: Option<Waker>,
    pub trailers: Option<HeaderMap>,
    pub done: bool,
    pub new: bool,
    pub thread: Option<Thread>,
}

impl Outgoing {
    pub fn wake(&self) {
        if let Some(waker) = &self.waker {
            waker.wake_by_ref();
        }
    }
}

impl Body for Outgoing {
    type Data = VecDeque<u8>;

    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let data = Pin::into_inner(self);

        if let Some(thread) = data.thread.take() {
            thread.unpark();
        }

        if !data.buf.is_empty() {
            return Poll::Ready(Some(Ok(Frame::data(std::mem::take(&mut data.buf)))));
        }

        if let Some(trailers) = data.trailers.take() {
            data.done = true;

            return Poll::Ready(Some(Ok(Frame::trailers(trailers))));
        }

        if data.done {
            return Poll::Ready(None);
        }

        data.waker = Some(cx.waker().clone());

        Poll::Pending
    }
}

impl wasi::http::types::HostOutgoingBody for State {
    fn write(
        &mut self,
        self_: Resource<OutgoingBody>,
    ) -> wasmtime::Result<Result<Resource<OutputStream>, ()>> {
        let resource = self
            .responses
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find body"))?
            .body_mut();

        if !resource.new {
            Ok(Err(()))
        } else {
            resource.new = false;

            Ok(Ok(Resource::new_own(self_.rep())))
        }
    }

    fn finish(
        &mut self,
        this: Resource<OutgoingBody>,
        trailers: Option<Resource<Trailers>>,
    ) -> wasmtime::Result<Result<(), ErrorCode>> {
        let resource = self
            .responses
            .get_mut(&this.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find body"))?
            .body_mut();

        resource.done = true;
        if let Some(trailers) = trailers {
            resource.trailers = Some(
                self.fields
                    .remove(&trailers.rep())
                    .ok_or_else(|| wasmtime::Error::msg("Could not find trailers"))?
                    .1,
            );
        }

        Ok(Ok(()))
    }

    fn drop(&mut self, _rep: Resource<OutgoingBody>) -> wasmtime::Result<()> {
        Ok(())
    }
}

struct TrailerPollable {
    id: u32,
}

impl PollableIndividual for TrailerPollable {
    fn ready(&mut self, state: &mut State) -> wasmtime::Result<bool> {
        let resource = state
            .incoming
            .get_mut(&self.id)
            .ok_or_else(|| wasmtime::Error::msg("Could not find body"))?;

        let Poll::Ready(res) =
            Pin::new(&mut resource.incoming).poll_frame(&mut Context::from_waker(noop_waker_ref()))
        else {
            return Ok(false);
        };

        if let Some(frame) = res {
            let frame = match frame {
                Ok(frame) => frame,
                Err(_) => {
                    return Ok(true);
                }
            };

            if frame.is_data() {
                return Ok(false);
            } else {
                let trailers = frame.into_trailers().unwrap();
                resource.trailers = Some(trailers);
                return Ok(true);
            }
        } else {
            resource.state = BodyState::Consumed;
            Ok(true)
        }
    }

    fn block(&mut self, state: &mut State) -> wasmtime::Result<()> {
        let resource = state
            .incoming
            .get_mut(&self.id)
            .ok_or_else(|| wasmtime::Error::msg("Could not find body"))?;

        loop {
            let res = futures::executor::block_on(poll_fn(|cx| {
                Pin::new(&mut resource.incoming).poll_frame(cx)
            }));

            if let Some(frame) = res {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(_) => {
                        return Ok(());
                    }
                };

                if frame.is_trailers() {
                    let trailers = frame.into_trailers().unwrap();
                    resource.trailers = Some(trailers);
                    return Ok(());
                }
            } else {
                resource.state = BodyState::Consumed;
                return Ok(());
            }
        }
    }
}

impl wasi::http::types::HostFutureTrailers for State {
    fn subscribe(
        &mut self,
        self_: Resource<FutureTrailers>,
    ) -> wasmtime::Result<Resource<Pollable>> {
        let id = self.new_id();

        self.pollables
            .insert(id, Box::new(TrailerPollable { id: self_.rep() }));

        Ok(Resource::new_own(id))
    }

    fn get(
        &mut self,
        self_: Resource<FutureTrailers>,
    ) -> wasmtime::Result<Option<Result<Option<Resource<Trailers>>, ErrorCode>>> {
        let id = self.new_id();

        let resource = self
            .incoming
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find the body"))?;

        if let Some(trailers) = resource.trailers.take() {
            self.fields.insert(id, (true, trailers));

            return Ok(Some(Ok(Some(Resource::new_own(id)))));
        }

        let Poll::Ready(res) =
            Pin::new(&mut resource.incoming).poll_frame(&mut Context::from_waker(noop_waker_ref()))
        else {
            return Ok(None);
        };

        if let Some(frame) = res {
            let frame = match frame {
                Ok(frame) => frame,
                Err(err) => {
                    return Ok(Some(Err(ErrorCode::InternalError(Some(format!(
                        "{}",
                        err
                    ))))));
                }
            };

            if frame.is_data() {
                return Ok(None);
            } else {
                let trailers = frame.into_trailers().unwrap();
                self.fields.insert(id, (true, trailers));
                return Ok(Some(Ok(Some(Resource::new_own(id)))));
            }
        } else {
            resource.state = BodyState::Consumed;
            Ok(Some(Ok(None)))
        }
    }

    fn drop(&mut self, _rep: Resource<FutureTrailers>) -> wasmtime::Result<()> {
        Ok(())
    }
}

impl wasi::http::types::HostOutgoingResponse for State {
    fn new(&mut self, headers: Resource<Headers>) -> wasmtime::Result<Resource<OutgoingResponse>> {
        let id = self.new_id();

        let mut response = Response::new(Outgoing {
            buf: VecDeque::new(),
            waker: None,
            trailers: None,
            done: false,
            new: true,
            thread: None,
        });

        let mut headers = self
            .fields
            .remove(&headers.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find headers"))?;

        std::mem::swap(response.headers_mut(), &mut headers.1);

        self.responses.insert(id, response);

        Ok(Resource::new_own(id))
    }

    fn status_code(&mut self, self_: Resource<OutgoingResponse>) -> wasmtime::Result<StatusCode> {
        let resource = self
            .responses
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find response"))?;

        Ok(resource.status().as_u16())
    }

    fn set_status_code(
        &mut self,
        self_: Resource<OutgoingResponse>,
        status_code: StatusCode,
    ) -> wasmtime::Result<Result<(), ()>> {
        let resource = self
            .responses
            .get_mut(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find response"))?;

        let status = resource.status_mut();

        *status = match http::StatusCode::try_from(status_code) {
            Ok(status) => status,
            Err(_) => return Ok(Err(())),
        };

        Ok(Ok(()))
    }

    fn headers(
        &mut self,
        self_: Resource<OutgoingResponse>,
    ) -> wasmtime::Result<Resource<Headers>> {
        let id = self.new_id();
        let resource = self
            .responses
            .get(&self_.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find response"))?;

        self.fields.insert(id, (true, resource.headers().clone()));

        Ok(Resource::new_own(id))
    }

    fn body(
        &mut self,
        self_: Resource<OutgoingResponse>,
    ) -> wasmtime::Result<Result<Resource<OutgoingBody>, ()>> {
        Ok(Ok(Resource::new_own(self_.rep()))) // TODO: Allow only one body
    }

    fn drop(&mut self, rep: Resource<OutgoingResponse>) -> wasmtime::Result<()> {
        self.responses.remove(&rep.rep());

        Ok(())
    }
}

impl wasi::http::types::HostResponseOutparam for State {
    fn set(
        &mut self,
        param: Resource<ResponseOutparam>,
        response: Result<Resource<OutgoingResponse>, ErrorCode>,
    ) -> wasmtime::Result<()> {
        let res = response.unwrap().rep();
        let resource = self
            .full_responses
            .get_mut(&param.rep())
            .ok_or_else(|| wasmtime::Error::msg("Could not find full response"))?;

        let response = self
            .responses
            .remove(&res)
            .ok_or_else(|| wasmtime::Error::msg("Could not find response"))?;

        *resource = Some(response);

        Ok(())
    }

    fn drop(&mut self, rep: Resource<ResponseOutparam>) -> wasmtime::Result<()> {
        self.full_responses.remove(&rep.rep());

        Ok(())
    }
}

impl wasi::http::types::HostRequestOptions for State {
    fn new(&mut self) -> wasmtime::Result<Resource<RequestOptions>> {
        unimplemented!();
    }

    fn connect_timeout_ms(
        &mut self,
        self_: Resource<RequestOptions>,
    ) -> wasmtime::Result<Option<Duration>> {
        unimplemented!();
    }

    fn set_connect_timeout_ms(
        &mut self,
        self_: Resource<RequestOptions>,
        ms: Option<Duration>,
    ) -> wasmtime::Result<Result<(), ()>> {
        unimplemented!();
    }

    fn first_byte_timeout_ms(
        &mut self,
        self_: Resource<RequestOptions>,
    ) -> wasmtime::Result<Option<Duration>> {
        unimplemented!();
    }

    fn set_first_byte_timeout_ms(
        &mut self,
        self_: Resource<RequestOptions>,
        ms: Option<Duration>,
    ) -> wasmtime::Result<Result<(), ()>> {
        unimplemented!();
    }

    fn between_bytes_timeout_ms(
        &mut self,
        self_: Resource<RequestOptions>,
    ) -> wasmtime::Result<Option<Duration>> {
        unimplemented!();
    }

    fn set_between_bytes_timeout_ms(
        &mut self,
        self_: Resource<RequestOptions>,
        ms: Option<Duration>,
    ) -> wasmtime::Result<Result<(), ()>> {
        unimplemented!();
    }

    fn drop(&mut self, rep: Resource<RequestOptions>) -> wasmtime::Result<()> {
        unimplemented!();
    }
}

impl wasi::http::types::HostOutgoingRequest for State {
    fn new(&mut self, headers: Resource<Headers>) -> wasmtime::Result<Resource<OutgoingRequest>> {
        unimplemented!()
    }

    fn body(
        &mut self,
        self_: Resource<OutgoingRequest>,
    ) -> wasmtime::Result<Result<Resource<OutgoingBody>, ()>> {
        unimplemented!()
    }

    fn method(&mut self, self_: Resource<OutgoingRequest>) -> wasmtime::Result<Method> {
        unimplemented!()
    }

    fn set_method(
        &mut self,
        self_: Resource<OutgoingRequest>,
        method: Method,
    ) -> wasmtime::Result<Result<(), ()>> {
        unimplemented!()
    }

    fn path_with_query(
        &mut self,
        self_: Resource<OutgoingRequest>,
    ) -> wasmtime::Result<Option<String>> {
        unimplemented!()
    }

    fn set_path_with_query(
        &mut self,
        self_: Resource<OutgoingRequest>,
        path_with_query: Option<String>,
    ) -> wasmtime::Result<Result<(), ()>> {
        unimplemented!()
    }

    fn scheme(&mut self, self_: Resource<OutgoingRequest>) -> wasmtime::Result<Option<Scheme>> {
        unimplemented!()
    }

    fn set_scheme(
        &mut self,
        self_: Resource<OutgoingRequest>,
        scheme: Option<Scheme>,
    ) -> wasmtime::Result<Result<(), ()>> {
        unimplemented!()
    }

    fn authority(&mut self, self_: Resource<OutgoingRequest>) -> wasmtime::Result<Option<String>> {
        unimplemented!()
    }

    fn set_authority(
        &mut self,
        self_: Resource<OutgoingRequest>,
        authority: Option<String>,
    ) -> wasmtime::Result<Result<(), ()>> {
        unimplemented!()
    }

    fn headers(&mut self, self_: Resource<OutgoingRequest>) -> wasmtime::Result<Resource<Headers>> {
        unimplemented!()
    }

    fn drop(&mut self, rep: Resource<OutgoingRequest>) -> wasmtime::Result<()> {
        unimplemented!()
    }
}

impl wasi::http::types::HostIncomingResponse for State {
    fn status(&mut self, self_: Resource<IncomingResponse>) -> wasmtime::Result<StatusCode> {
        unimplemented!()
    }

    fn headers(
        &mut self,
        self_: Resource<IncomingResponse>,
    ) -> wasmtime::Result<Resource<Headers>> {
        unimplemented!()
    }

    fn consume(
        &mut self,
        self_: Resource<IncomingResponse>,
    ) -> wasmtime::Result<Result<Resource<IncomingBody>, ()>> {
        unimplemented!()
    }

    fn drop(&mut self, rep: Resource<IncomingResponse>) -> wasmtime::Result<()> {
        unimplemented!()
    }
}

impl wasi::http::types::HostFutureIncomingResponse for State {
    fn subscribe(
        &mut self,
        self_: Resource<FutureIncomingResponse>,
    ) -> wasmtime::Result<Resource<Pollable>> {
        unimplemented!()
    }

    fn get(
        &mut self,
        self_: Resource<FutureIncomingResponse>,
    ) -> wasmtime::Result<Option<Result<Result<Resource<IncomingResponse>, ErrorCode>, ()>>> {
        unimplemented!()
    }

    fn drop(&mut self, rep: Resource<FutureIncomingResponse>) -> wasmtime::Result<()> {
        unimplemented!()
    }
}
