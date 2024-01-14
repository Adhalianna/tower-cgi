use std::error::Error;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use bytes::Buf;
use futures::{Future, FutureExt, StreamExt};
use http::uri::{Authority, Scheme};
use http::{header, Request, Response, StatusCode};
use http_body::Body;
use http_body_util::{BodyStream, StreamBody};
use tokio::io::{self, AsyncBufReadExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio_util::io::{ReaderStream, StreamReader};
use tower::Service;

#[derive(Clone, Debug)]
pub struct Cgi {
    path: PathBuf,
    env_clear: bool,
}

impl Cgi {
    pub fn new<P: AsRef<Path>>(path: P) -> Self {
        Cgi {
            path: path.as_ref().to_path_buf(),
            env_clear: true,
        }
    }

    pub fn env_clear(mut self, clear: bool) -> Self {
        self.env_clear = clear;
        self
    }
}

type BoxedError = Box<dyn Error + Sync + Send>;
pub type CgiResponse = Response<StreamBody<ReaderStream<BufReader<ChildStdout>>>>;

impl<B> Service<Request<B>> for Cgi
where
    B: Body + Send + Unpin + 'static,
    <B as Body>::Data: Buf + Send,
    <B as Body>::Error: Error + Send + Sync,
{
    type Response = Response<StreamBody<ReaderStream<BufReader<ChildStdout>>>>;
    type Error = BoxedError;

    #[allow(clippy::type_complexity)]
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    #[inline]
    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let script_path = self.path.clone();
        let env_clear = self.env_clear;

        async move {
            let script_path = std::fs::canonicalize(script_path)?;

            let mut cmd = Command::new(&script_path);
            let cmd = if env_clear { cmd.env_clear() } else { &mut cmd };

            let mut child = cmd
                .env("GATEWAY_INTERFACE", "CGI/1.1")
                .env("QUERY_STRING", req.uri().query().unwrap_or_default())
                .env("PATH_INFO", req.uri().path())
                .env("PATH_TRANSLATED", &script_path)
                .env("REQUEST_METHOD", req.method().as_str().to_ascii_uppercase())
                //TODO: clean up below
                .env("REMOTE_HOST", req.uri().host().unwrap_or("NULL"))
                .env(
                    "REMOTE_ADDR",
                    req.headers()
                        .get("X-Forwarded-For")
                        .map(|h| h.to_str())
                        .transpose()
                        .unwrap_or(Some("NULL"))
                        .unwrap_or_else(|| {
                            req.headers()
                                .get("Forwarded")
                                .map(|h| h.to_str())
                                .transpose()
                                .unwrap_or(Some("NULL"))
                                .unwrap_or("NULL")
                        }),
                )
                .env("REMOTE_USER", {
                    req.uri()
                        .authority()
                        .map(|atrty| atrty.as_str())
                        .map(|atrty| {
                            let user_and_maybe_pass =
                                atrty.split_once('@').unwrap_or((atrty, "")).0;
                            let user = user_and_maybe_pass
                                .split_once(':')
                                .unwrap_or((user_and_maybe_pass, ""))
                                .0;
                            user.to_owned()
                        })
                        .unwrap_or_else(|| {
                            req.headers()
                                .get("WWW-Authenticate")
                                .and_then(|h| {
                                    let user_id = http_auth_basic::Credentials::from_header(
                                        h.to_str().unwrap_or("").to_owned(),
                                    )
                                    .map_or("NULL".to_owned(), |creds| creds.user_id);
                                    Some(user_id)
                                })
                                .unwrap_or("NULL".to_owned())
                        })
                })
                //TODO: clean up above
                .env("SCRIPT_NAME", req.uri().path())
                .env(
                    "SERVER_NAME",
                    req.headers()
                        .get(header::HOST)
                        .and_then(|val| val.to_str().ok())
                        .and_then(|host| host.parse::<Authority>().ok())
                        .map(|authority| authority.host().to_owned())
                        .unwrap_or_default(),
                )
                .env(
                    "SERVER_PORT",
                    req.uri()
                        .port()
                        .map(|port| port.to_string())
                        .or_else(|| {
                            req.headers().get("x-forwarded-proto").and_then(|val| {
                                match val.to_str() {
                                    Ok("http") => Some("80".to_string()),
                                    Ok("https") => Some("443".to_string()),
                                    _ => None,
                                }
                            })
                        })
                        .or_else(|| match req.uri().scheme() {
                            Some(scheme) if *scheme == Scheme::HTTP => Some("80".to_string()),
                            Some(scheme) if *scheme == Scheme::HTTPS => Some("443".to_string()),
                            _ => None,
                        })
                        .unwrap_or_else(|| "80".to_string()),
                )
                .env("SERVER_PROTOCOL", format!("{:?}", req.version()))
                .env("SERVER_SOFTWARE", "tower-cgi/0.0.1")
                .env(
                    "CONTENT_TYPE",
                    req.headers()
                        .get(header::CONTENT_TYPE)
                        .and_then(|val| val.to_str().ok())
                        .unwrap_or_default(),
                )
                .env(
                    "CONTENT_LENGTH",
                    req.headers()
                        .get(header::CONTENT_LENGTH)
                        .and_then(|val| val.to_str().ok())
                        .unwrap_or_default(),
                )
                .envs(
                    req.headers()
                        .into_iter()
                        .map(|(name, value)| {
                            let name = format!("HTTP_{}", name)
                                .replace("-", "_")
                                .to_ascii_uppercase();
                            Ok((name, value.to_str()?))
                        })
                        .collect::<Result<Vec<_>, Self::Error>>()?,
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()?;

            let mut stdin = child.stdin.take().ok_or("Failed to get process STDIN")?;
            let stdout = child.stdout.take().ok_or("Failed to get process STDOUT")?;

            tokio::spawn(async move { child.wait().await.unwrap() });

            let write_request_body = async move {
                let request_body = req.into_body();

                let mut request_body_reader = StreamReader::new(
                    BodyStream::new(request_body)
                        .map(|chunk| {
                            chunk.map_err(|err| io::Error::other(err)).map(|frame| {
                                frame
                                    .into_data()
                                    .map_err(|_| io::Error::other("failed to read DATA frame"))
                            })
                        })
                        .map(|res| match res {
                            Ok(res) => res,
                            Err(err) => Err(err),
                        }),
                );
                io::copy(&mut request_body_reader, &mut stdin).await?;
                Ok::<_, Self::Error>(io::copy(&mut request_body_reader, &mut stdin).await?)
            };

            let read_response = async move {
                let mut stdout_reader = BufReader::new(stdout);
                let mut headers = Vec::new();

                loop {
                    stdout_reader.read_until(b'\n', &mut headers).await?;

                    match headers.as_slice() {
                        [.., b'\r', b'\n', b'\r', b'\n'] => break,
                        [.., b'\n', b'\n'] => break,
                        _ => continue,
                    }
                }

                let mut parsed_headers = [httparse::EMPTY_HEADER; 64];
                httparse::parse_headers(&headers, &mut parsed_headers)?;

                let response = parsed_headers
                    .into_iter()
                    .filter(|header| *header != httparse::EMPTY_HEADER)
                    .map(|header| (header.name, header.value))
                    .try_fold(
                        Response::builder().status(200),
                        |response, (name, value)| {
                            if name.to_ascii_lowercase() == "status" {
                                Ok::<_, Self::Error>(
                                    response.status(StatusCode::from_bytes(&value[0..3])?),
                                )
                            } else {
                                Ok(response.header(name, value))
                            }
                        },
                    )?;

                let body_reader = ReaderStream::new(stdout_reader);
                let response = response.body(http_body_util::StreamBody::new(body_reader))?;
                Ok::<_, Self::Error>(response)
            };

            let (_, response) = tokio::try_join!(write_request_body, read_response)?;

            Ok(response)
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use std::fs::Permissions;
    use std::io;
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use http::Request;
    use indoc::indoc;
    use tempfile::{NamedTempFile, TempPath};
    use tokio::io::AsyncReadExt;
    use tokio_util::io::StreamReader;
    use tower::ServiceExt;

    use crate::Cgi;
    use crate::CgiResponse;

    async fn temp_cgi_script(program: &str) -> io::Result<TempPath> {
        let mut file = NamedTempFile::new()?;
        file.as_file_mut()
            .set_permissions(Permissions::from_mode(0o755))?;
        writeln!(file, "{}", program)?;
        let path = file.into_temp_path();
        // Just to eliminate possible heisen tests when the OS didn't
        // close the file and we tried to execute it.
        std::thread::sleep(std::time::Duration::from_secs(1));
        Ok(path)
    }

    #[tokio::test]
    async fn test_status_code() {
        let script = temp_cgi_script(indoc! {r#"
            #!/bin/sh
            echo "Status: 201 Created"
            echo ""
        "#})
        .await
        .unwrap();

        let svc = Cgi::new(&script);

        let req = Request::builder()
            .body(http_body_util::Empty::<&[u8]>::new())
            .unwrap();
        let res = svc.oneshot(req).await.unwrap();

        assert_eq!(res.status(), 201);
    }

    #[tokio::test]
    async fn test_response_headers() {
        let script = temp_cgi_script(indoc! {r#"
            #!/bin/sh
            echo "Status: 200"
            echo "x-some-header: hello"
            echo "x-other-header: bye"
            echo ""
        "#})
        .await
        .unwrap();

        let svc = Cgi::new(&script);

        let req = Request::builder()
            .body(http_body_util::Empty::<&[u8]>::new())
            .unwrap();
        let res = svc.oneshot(req).await.unwrap();

        assert_eq!(res.headers()["x-some-header"], "hello");
        assert_eq!(res.headers()["x-other-header"], "bye");
    }

    #[tokio::test]
    async fn test_request_headers() {
        let script = temp_cgi_script(indoc! {r#"
            #!/bin/sh
            echo "Status: 200"
            echo "x-req-header: ${HTTP_SOME_REQUEST_HEADER}"
            echo ""
        "#})
        .await
        .unwrap();

        let svc = Cgi::new(&script);

        let req = Request::builder()
            .header("some-request-header", "hello")
            .body(http_body_util::Empty::<&[u8]>::new())
            .unwrap();

        let res = svc.oneshot(req).await.unwrap();

        assert_eq!(res.headers()["x-req-header"], "hello");
    }

    #[tokio::test]
    async fn test_response_body() {
        let script = temp_cgi_script(indoc! {r#"
            #!/bin/sh
            echo "Status: 200"
            echo ""
            printf "Hello"
        "#})
        .await
        .unwrap();

        let svc = Cgi::new(&script);

        let req = Request::builder()
            .body(http_body_util::Empty::<&[u8]>::new())
            .unwrap();

        let res: CgiResponse = svc.oneshot(req).await.unwrap();
        let mut buf = Vec::<u8>::with_capacity(5);
        let mut body = StreamReader::new(res.into_body());
        body.read_to_end(&mut buf).await.unwrap();
        dbg!(&body);

        assert_eq!(&buf[..], b"Hello");
    }

    #[tokio::test]
    async fn test_request_body() {
        let script = temp_cgi_script(indoc! {r#"
            #!/bin/sh
            echo "Status: 200"
            echo ""
            cat -
        "#})
        .await
        .unwrap();

        let svc = Cgi::new(&script);

        let input = b"input";

        let req = Request::builder()
            .body(http_body_util::Full::new(&input[..]))
            .unwrap();

        let res: CgiResponse = svc.oneshot(req).await.unwrap();
        let mut buf = Vec::<u8>::with_capacity(5);
        let mut body = StreamReader::new(res.into_body());
        body.read_to_end(&mut buf).await.unwrap();

        assert_eq!(&buf[..], input);
    }
}
