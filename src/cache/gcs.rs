// Copyright 2017 Mozilla Foundation
// Copyright 2017 Google Inc.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cell::RefCell;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::rc::Rc;
use std::time;

use cache::{
    Cache,
    CacheRead,
    CacheWrite,
    Storage,
};
use chrono;
use futures::future::Shared;
use futures::{future, Async, Future, Stream};
use hyper;
use hyper::header::{Authorization, Bearer, ContentType, ContentLength};
use hyper::Method;
use hyper::client::{Client, HttpConnector, Request};
use hyper_tls::HttpsConnector;
use jwt;
use openssl;
use serde_json;
use tokio_core::reactor::Handle;
use url::form_urlencoded;
use url::percent_encoding::{percent_encode, PATH_SEGMENT_ENCODE_SET, QUERY_ENCODE_SET};

use errors::*;

type HyperClient = Client<HttpsConnector<HttpConnector>>;

/// A GCS bucket
struct Bucket {
    name: String,
    base_url: String,
    client: HyperClient,
}

impl fmt::Display for Bucket {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Bucket(name={}, base_url={})", self.name, self.base_url)
    }
}

impl Bucket {
    pub fn new(name: String, base_url: String, handle: &Handle) -> Result<Bucket> {
        let client = Client::configure()
                        .connector(HttpsConnector::new(1, handle)?)
                        .build(handle);

        Ok(Bucket { name, base_url, client })
    }

    fn get(&self, key: &str, cred_provider: &GCSCredentialProvider) -> SFuture<Vec<u8>> {
        let url = format!("{}/download/storage/v1/b/{}/o/{}?alt=media",
                    self.base_url,
                    percent_encode(self.name.as_bytes(), PATH_SEGMENT_ENCODE_SET),
                    percent_encode(key.as_bytes(), PATH_SEGMENT_ENCODE_SET));

        let client = self.client.clone();

        Box::new(cred_provider.credentials(&self.client).and_then(move |creds| {
            let mut request = Request::new(Method::Get, url.parse().unwrap());
            request.headers_mut()
                .set(Authorization(Bearer { token: creds.token }));
            client.request(request).chain_err(move || {
                format!("failed GET: {}", url)
            }).and_then(|res| {
                if res.status().is_success() {
                    Ok(res.body())
                } else {
                    Err(ErrorKind::BadHTTPStatus(res.status().clone()).into())
                }
            }).and_then(|body| {
                body.fold(Vec::new(), |mut body, chunk| {
                    body.extend_from_slice(&chunk);
                    Ok::<_, hyper::Error>(body)
                }).chain_err(|| {
                    "failed to read HTTP body"
                })
            })
        }))
    }

    fn put(&self, key: &str, content: Vec<u8>, cred_provider: &GCSCredentialProvider) -> SFuture<()> {
        let url = format!("{}/upload/storage/v1/b/{}/o?name={}&uploadType=media",
                    self.base_url,
                    percent_encode(self.name.as_bytes(), PATH_SEGMENT_ENCODE_SET),
                    percent_encode(key.as_bytes(), QUERY_ENCODE_SET));

        let client = self.client.clone();

        Box::new(cred_provider.credentials(&client).and_then(move |creds| {
            let mut request = Request::new(Method::Post, url.parse().unwrap());
            {
                let mut headers = request.headers_mut();
                headers.set(Authorization(Bearer { token: creds.token }));
                headers.set(ContentType("application/octet-stream".parse().unwrap()));
                headers.set(ContentLength(content.len() as u64));
            }
            request.set_body(content);

            client.request(request).then(|result| {
                match result {
                    Ok(res) => {
                        if res.status().is_success() {
                            trace!("PUT succeeded");
                            Ok(())
                        } else {
                            trace!("PUT failed with HTTP status: {}", res.status());
                            Err(ErrorKind::BadHTTPStatus(res.status().clone()).into())
                        }
                    }
                    Err(e) => {
                        trace!("PUT failed with error: {:?}", e);
                        Err(e.into())
                    }
                }
            })
        }))
    }
}

pub struct GCSCredentialProvider {
    read_only: bool,
    credentials_path: String,
    cached_credentials: RefCell<Option<Shared<SFuture<GCSCredential>>>>,
}

#[derive(Debug, Deserialize)]
struct ServiceAccountKey {
    #[serde(rename = "type")]
    _type: String,
    project_id: String,
    private_key_id: String,
    private_key: String,
    client_email: String,
    client_id: String,
    auth_uri: String,
    token_uri: String,
    auth_provider_x509_cert_url: String,
}

#[derive(Serialize)]
struct JwtClaims {
    #[serde(rename = "iss")]
    issuer: String,
    scope: String,
    #[serde(rename = "aud")]
    audience: String,
    #[serde(rename = "exp")]
    expiration: i64,
    #[serde(rename = "iat")]
    issued_at: i64,
}

#[derive(Deserialize)]
struct TokenMsg {
    access_token: String,
    token_type: String,
    expires_in: usize,
}

#[derive(Clone)]
pub struct GCSCredential {
    token: String,
    expiration_time: chrono::DateTime<chrono::UTC>,
}

impl GCSCredentialProvider {
    pub fn new(read_only: bool, credentials_path: String) -> Self {
        GCSCredentialProvider {
            read_only,
            credentials_path,
            cached_credentials: RefCell::new(None),
        }
    }

    fn auth_request_jwt(&self, expire_at: &chrono::DateTime<chrono::UTC>) -> Result<String> {
        let metadata = fs::metadata(&self.credentials_path).chain_err(|| {
            "Couldn't stat GCS credentials file"
        })?;
        if !metadata.is_file() {
            bail!("Couldn't open GCS credentials file.");
        }
        let mut file = File::open(&self.credentials_path)?;
        let mut service_account_json = String::new();
        file.read_to_string(&mut service_account_json)?;
        let sa_key: ServiceAccountKey = serde_json::from_str(&service_account_json)?;

        let scope = (if self.read_only {
            "https://www.googleapis.com/auth/devstorage.readonly"
        } else {
            "https://www.googleapis.com/auth/devstorage.read_write"
        }).to_owned();

        let jwt_claims = JwtClaims {
            issuer: sa_key.client_email,
            scope: scope,
            audience: "https://www.googleapis.com/oauth2/v4/token".to_owned(),
            expiration: expire_at.timestamp(),
            issued_at: chrono::UTC::now().timestamp(),
        };

        let binary_key = openssl::rsa::Rsa::private_key_from_pem(
            sa_key.private_key.as_bytes()
        )?.private_key_to_der()?;

        let auth_request_jwt = jwt::encode(
            &jwt::Header::new(jwt::Algorithm::RS256),
            &jwt_claims,
            &binary_key,
        )?;

        Ok(auth_request_jwt)
    }

    pub fn credentials(&self, client: &HyperClient) -> SFuture<GCSCredential> {
        let mut future_opt = self.cached_credentials.borrow_mut();

        let needs_refresh = match Option::as_mut(&mut future_opt).map(|mut f| f.poll()) {
            None => true,
            Some(Ok(Async::Ready(ref creds))) => creds.expiration_time < chrono::UTC::now(),
            _ => false
        };

        if needs_refresh {
            let client = client.clone();
            let expires_at = chrono::UTC::now() + chrono::Duration::minutes(59);
            let auth_jwt = self.auth_request_jwt(&expires_at);
            let credentials: SFuture<_> = Box::new(future::result(auth_jwt).and_then(move |auth_jwt| {
                let url = "https://www.googleapis.com/oauth2/v4/token";
                let params = form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer")
                    .append_pair("assertion", &auth_jwt)
                    .finish();

                let mut request = Request::new(Method::Post, url.parse().unwrap());
                {
                    let mut headers = request.headers_mut();
                    headers.set(ContentType("application/x-www-form-urlencoded".parse().unwrap()));
                    headers.set(ContentLength(params.len() as u64));
                }
                request.set_body(params);

                client.request(request).map_err(Into::into)
            }).and_then(move |res| {
                if res.status().is_success() {
                    Ok(res.body())
                } else {
                    Err(ErrorKind::BadHTTPStatus(res.status().clone()).into())
                }
            }).and_then(move |body| {
                body.fold(Vec::new(), |mut body, chunk| {
                    body.extend_from_slice(&chunk);
                    Ok::<_, hyper::Error>(body)
                }).chain_err(|| {
                    "failed to read HTTP body"
                })
            }).and_then(move |body| {
                let body_str = String::from_utf8(body)?;
                let token_msg: TokenMsg = serde_json::from_str(&body_str)?;
                Ok(GCSCredential {
                    token: token_msg.access_token,
                    expiration_time: expires_at,
                })
            }));

            *future_opt = Some(credentials.shared());
        };

        Box::new(Option::as_mut(&mut future_opt).unwrap().clone().then(|result| {
            match result {
                Ok(e) => Ok((*e).clone()),
                Err(e) => Err(e.to_string().into()),
            }
        }))
    }
}

/// A cache that stores entries in Google Cloud Storage
pub struct GCSCache {
    /// The GCS bucket
    bucket: Rc<Bucket>,
    /// Credential provider for GCS
    credential_provider: GCSCredentialProvider,
}

impl GCSCache {
    /// Create a new `GCSCache` storing data in `bucket`
    pub fn new(bucket: String,
               endpoint: String,
               credential_provider: GCSCredentialProvider,
               handle: &Handle) -> Result<GCSCache>
    {
        Ok(GCSCache {
            bucket: Rc::new(Bucket::new(bucket, endpoint, handle)?),
            credential_provider: credential_provider,
        })
    }
}

impl Storage for GCSCache {
    fn get(&self, key: &str) -> SFuture<Cache> {
        Box::new(self.bucket.get(&key, &self.credential_provider).then(|result| {
            match result {
                Ok(data) => {
                    let hit = CacheRead::from(io::Cursor::new(data))?;
                    Ok(Cache::Hit(hit))
                }
                Err(e) => {
                    warn!("Got GCS error: {:?}", e);
                    Ok(Cache::Miss)
                }
            }
        }))
    }

    fn put(&self, key: &str, entry: CacheWrite) -> SFuture<time::Duration> {
        let start = time::Instant::now();
        let data = match entry.finish() {
            Ok(data) => data,
            Err(e) => return future::err(e.into()).boxed(),
        };
        let bucket = self.bucket.clone();
        let response = bucket.put(&key, data, &self.credential_provider).chain_err(|| {
            "failed to put cache entry in GCS"
        });

        Box::new(response.map(move |_| start.elapsed()))
    }

    fn location(&self) -> String {
        format!("GCS, bucket: {}", self.bucket)
    }

    fn current_size(&self) -> Option<usize> { None }
    fn max_size(&self) -> Option<usize> { None }
}
