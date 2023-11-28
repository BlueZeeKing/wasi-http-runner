use std::{
    pin::Pin,
    str::FromStr,
    task::{Context, Poll},
    thread,
};

use anyhow::anyhow;
use axum::{routing::get, Router};
use bytes::{Buf, Bytes};
use exports::wasi::http::incoming_handler::Guest;
use futures::{future::poll_fn, task::noop_waker_ref};
use http::{uri::Scheme, HeaderMap, HeaderName, HeaderValue, Request, Response, Uri};
use http_body::{Body, Frame};
use tower::{Service, ServiceExt};
use wasi::http::types::{
    ErrorCode, Fields, FutureTrailers, IncomingBody, IncomingRequest, InputStream, OutgoingBody,
    OutgoingResponse, ResponseOutparam,
};

wit_bindgen::generate!({
    world: "service",
    exports: {
        "wasi:http/incoming-handler": MyHost
    }
});

struct MyHost;

fn service() -> impl Service<
    Request<Incoming>,
    Response = Response<impl Body<Data = Bytes, Error = impl Into<anyhow::Error>>>,
    Error = impl Into<anyhow::Error>,
> {
    Router::new().route("/", get("Hello, World!"))
}

fn handle(request: IncomingRequest) -> anyhow::Result<OutgoingResponse> {
    let mut uri = Uri::builder();

    if let Some(scheme) = request.scheme() {
        let scheme: http::uri::Scheme = scheme.try_into()?;
        uri = uri.scheme(scheme);
    }

    if let Some(path) = request.path_with_query() {
        uri = uri.path_and_query(path);
    }

    if let Some(authority) = request.authority() {
        uri = uri.authority(authority);
    }

    let method: http::Method = request.method().try_into()?;

    let mut new_request = Request::builder().uri(uri.build()?).method(method);

    let headers = new_request
        .headers_mut()
        .ok_or(anyhow!("Could not find headers"))?;

    for (key, value) in request.headers().entries() {
        headers.append(
            HeaderName::from_str(&key)?,
            HeaderValue::from_bytes(&value)?,
        );
    }

    let request = new_request.body(Incoming::new(
        request
            .consume()
            .map_err(|_| anyhow!("Could not get request body"))?,
    ))?;

    let mut service = service();

    let router = match futures::executor::block_on(service.ready()) {
        Ok(v) => v,
        Err(e) => return Err(e.into()),
    };

    let response = match futures::executor::block_on(router.call(request)) {
        Ok(v) => v,
        Err(e) => return Err(e.into()),
    };

    let fields = response
        .headers()
        .iter()
        .map(|(key, value)| (key.to_string(), value.as_bytes().to_vec()))
        .collect::<Vec<_>>();

    let new_response = OutgoingResponse::new(Fields::from_list(&fields)?);

    new_response
        .set_status_code(response.status().as_u16())
        .map_err(|_| anyhow!("Could not set status code"))?;

    let outgoing_body = new_response
        .body()
        .map_err(|_| anyhow!("Could not get body"))?;

    let output = outgoing_body
        .write()
        .map_err(|_| anyhow!("Could not get stream"))?;

    let mut body = response.into_body();

    let trailers = loop {
        let data = Pin::new(&mut body).poll_frame(&mut Context::from_waker(noop_waker_ref()));

        output.subscribe().block();
        let mut amount = output.check_write()?;

        let data = match data {
            Poll::Pending => {
                futures::executor::block_on(poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)))
            }
            Poll::Ready(val) => val,
        };

        if let Some(frame) = data {
            let frame = match frame {
                Ok(v) => v,
                Err(err) => return Err(err.into()),
            };
            if frame.is_data() {
                let mut data = frame.into_data().map_err(|_| anyhow!("Unreachable"))?;
                while data.has_remaining() {
                    let mut remaining = data.split_off((amount as usize).min(data.len()));
                    std::mem::swap(&mut data, &mut remaining);

                    output.write(&remaining)?;
                    output.subscribe().block();
                    amount = output.check_write()?;
                }
            } else {
                let trailers = frame.into_trailers().map_err(|_| anyhow!("Unreachable"))?;
                break Some(trailers);
            }
        } else {
            break None;
        }
    };

    drop(output);

    OutgoingBody::finish(
        outgoing_body,
        trailers.and_then(|val| {
            let entries = val
                .iter()
                .map(|(key, value)| (key.to_string(), value.as_bytes().to_vec()))
                .collect::<Vec<_>>();

            Fields::from_list(&entries).ok()
        }),
    )?;

    Ok(new_response)
}

impl Guest for MyHost {
    fn handle(request: IncomingRequest, response_out: ResponseOutparam) {
        let res = handle(request);
        ResponseOutparam::set(
            response_out,
            res.map_err(|_| &ErrorCode::InternalError(None)),
        );
    }
}

impl TryInto<http::uri::Scheme> for wasi::http::types::Scheme {
    type Error = anyhow::Error;

    fn try_into(self) -> Result<http::uri::Scheme, Self::Error> {
        Ok(match self {
            wasi::http::types::Scheme::Http => Scheme::HTTP,
            wasi::http::types::Scheme::Https => Scheme::HTTPS,
            wasi::http::types::Scheme::Other(val) => Scheme::try_from(val.as_str())?,
        })
    }
}

impl TryInto<http::Method> for wasi::http::types::Method {
    type Error = anyhow::Error;

    fn try_into(self) -> Result<http::Method, Self::Error> {
        Ok(match self {
            wasi::http::types::Method::Get => http::Method::GET,
            wasi::http::types::Method::Head => http::Method::HEAD,
            wasi::http::types::Method::Post => http::Method::POST,
            wasi::http::types::Method::Put => http::Method::PUT,
            wasi::http::types::Method::Delete => http::Method::DELETE,
            wasi::http::types::Method::Connect => http::Method::CONNECT,
            wasi::http::types::Method::Options => http::Method::OPTIONS,
            wasi::http::types::Method::Trace => http::Method::TRACE,
            wasi::http::types::Method::Patch => http::Method::PATCH,
            wasi::http::types::Method::Other(s) => http::Method::from_str(s.as_str())?,
        })
    }
}

impl Incoming {
    pub fn new(body: IncomingBody) -> Self {
        Self {
            body: Some(body),
            stream: None,
            trailers: None,
            stream_gone: false,
        }
    }
}

struct Incoming {
    body: Option<IncomingBody>,
    stream: Option<InputStream>,
    trailers: Option<FutureTrailers>,
    stream_gone: bool,
}

impl Body for Incoming {
    type Data = Bytes;
    type Error = anyhow::Error;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<http_body::Frame<Self::Data>, Self::Error>>> {
        let data = Pin::into_inner(self);

        if let Some(stream) = data.stream.as_ref() {
            let result = stream.read(4096);

            match result {
                Ok(val) => {
                    if val.len() == 0 {
                        let pollable = stream.subscribe();
                        let waker = cx.waker().clone();

                        thread::spawn(move || {
                            pollable.block();
                            waker.wake();
                        });

                        Poll::Pending
                    } else {
                        Poll::Ready(Some(Ok(Frame::data(Bytes::from(val)))))
                    }
                }
                Err(wasi::io::streams::StreamError::Closed) => {
                    data.stream_gone = true;
                    data.stream = None;
                    Pin::new(data).poll_frame(cx)
                }
                Err(wasi::io::streams::StreamError::LastOperationFailed(err)) => {
                    Poll::Ready(Some(Err(anyhow::anyhow!(err.to_debug_string()))))
                }
            }
        } else if let Some(trailer) = data.trailers.as_ref() {
            let result = trailer.get();

            match result {
                Some(Ok(Some(trailers))) => {
                    let mut headers = HeaderMap::new();

                    for (key, value) in trailers.entries() {
                        headers.append(
                            HeaderName::from_str(&key)?,
                            HeaderValue::from_bytes(&value)?,
                        );
                    }

                    Poll::Ready(Some(Ok(Frame::trailers(headers))))
                }
                Some(Ok(None)) => Poll::Ready(None),
                Some(Err(err)) => Poll::Ready(Some(Err(anyhow!(err.to_string())))),
                None => {
                    let pollable = trailer.subscribe();
                    let waker = cx.waker().clone();

                    thread::spawn(move || {
                        pollable.block();
                        waker.wake();
                    });

                    Poll::Pending
                }
            }
        } else if data.stream_gone {
            data.trailers = Some(IncomingBody::finish(match data.body.take() {
                Some(v) => v,
                None => return Poll::Ready(Some(Err(anyhow!("Could not find body")))),
            }));
            Pin::new(data).poll_frame(cx)
        } else {
            data.stream = Some(
                match match data.body.as_ref() {
                    Some(v) => v,
                    None => return Poll::Ready(Some(Err(anyhow!("Could not find body")))),
                }
                .stream()
                {
                    Ok(v) => v,
                    Err(_) => return Poll::Ready(Some(Err(anyhow!("Could not find stream")))),
                },
            );
            Pin::new(data).poll_frame(cx)
        }
    }
}
