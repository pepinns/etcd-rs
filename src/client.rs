use std::{future::Future, sync::Arc, time::Duration};

use tokio::sync::{mpsc::channel, RwLock};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{
    metadata::{Ascii, MetadataValue},
    transport::Channel,
    Status,
};

use crate::{
    auth::{AuthDisableRequest, AuthEnableRequest, AuthRoleListRequest},
    proto::etcdserverpb::LeaseKeepAliveRequest,
};
use crate::{
    auth::{AuthOp, AuthenticateResponse},
    cluster::{
        ClusterOp, MemberAddRequest, MemberAddResponse, MemberListRequest, MemberListResponse,
        MemberRemoveRequest, MemberRemoveResponse, MemberUpdateRequest, MemberUpdateResponse,
    },
    kv::{
        CompactRequest, CompactResponse, DeleteRequest, DeleteResponse, KeyRange, KeyValueOp,
        PutRequest, PutResponse, RangeRequest, RangeResponse, TxnRequest, TxnResponse,
    },
    lease::{
        LeaseGrantRequest, LeaseGrantResponse, LeaseId, LeaseKeepAlive, LeaseOp,
        LeaseRevokeRequest, LeaseRevokeResponse, LeaseTimeToLiveRequest, LeaseTimeToLiveResponse,
    },
    proto::etcdserverpb,
    proto::etcdserverpb::cluster_client::ClusterClient,
    proto::etcdserverpb::{
        auth_client::AuthClient, kv_client::KvClient, lease_client::LeaseClient,
        watch_client::WatchClient,
    },
    watch::{WatchCanceler, WatchCreateRequest, WatchOp, WatchStream},
    AuthDisableResponse, AuthEnableResponse, AuthRoleAddRequest, AuthRoleAddResponse,
    AuthRoleDeleteRequest, AuthRoleDeleteResponse, AuthRoleListResponse, AuthStatusRequest,
    AuthStatusResponse, AuthenticateRequest, Error, Result,
};

static MAX_RETRY: i32 = 3;

#[derive(Debug, Clone)]
pub struct Endpoint {
    url: String,
    #[cfg(feature = "tls")]
    tls_opt: Option<tonic::transport::ClientTlsConfig>,
}

impl Endpoint {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            #[cfg(feature = "tls")]
            tls_opt: None,
        }
    }

    #[cfg(feature = "tls")]
    pub fn tls_raw(
        mut self,
        domain_name: impl Into<String>,
        ca_cert: impl AsRef<[u8]>,
        client_cert: impl AsRef<[u8]>,
        client_key: impl AsRef<[u8]>,
    ) -> Self {
        use tonic::transport::{Certificate, ClientTlsConfig, Identity};

        let certificate = Certificate::from_pem(ca_cert);
        let identity = Identity::from_pem(client_cert, client_key);

        self.tls_opt = Some(
            ClientTlsConfig::new()
                .domain_name(domain_name)
                .ca_certificate(certificate)
                .identity(identity),
        );

        self
    }

    #[cfg(feature = "tls")]
    pub async fn tls(
        self,
        domain_name: impl Into<String>,
        ca_cert_path: impl AsRef<std::path::Path>,
        client_cert_path: impl AsRef<std::path::Path>,
        client_key_path: impl AsRef<std::path::Path>,
    ) -> Result<Self> {
        use tokio::fs::read;

        let ca_cert = read(ca_cert_path).await?;
        let client_cert = read(client_cert_path).await?;
        let client_key = read(client_key_path).await?;

        Ok(self.tls_raw(domain_name, ca_cert, client_cert, client_key))
    }
}

impl<T> From<T> for Endpoint
where
    T: Into<String>,
{
    fn from(url: T) -> Self {
        Self {
            url: url.into(),
            #[cfg(feature = "tls")]
            tls_opt: None,
        }
    }
}

/// Config for establishing etcd client.
#[derive(Clone, Debug)]
pub struct ClientConfig {
    pub endpoints: Vec<Endpoint>,
    pub auth: Option<(String, String)>,
    pub connect_timeout: Duration,
    pub http2_keep_alive_interval: Duration,
}

impl ClientConfig {
    pub fn new(endpoints: impl Into<Vec<Endpoint>>) -> Self {
        Self {
            endpoints: endpoints.into(),
            auth: None,
            connect_timeout: Duration::from_secs(30),
            http2_keep_alive_interval: Duration::from_secs(5),
        }
    }

    pub fn auth(mut self, name: impl Into<String>, password: impl Into<String>) -> Self {
        self.auth = Some((name.into(), password.into()));
        self
    }

    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    pub fn http2_keep_alive_interval(mut self, interval: Duration) -> Self {
        self.http2_keep_alive_interval = interval;
        self
    }
}

/// Client is an abstraction for grouping etcd operations and managing underlying network communications.
#[derive(Clone)]
pub struct Client {
    auth_client: AuthClient<Channel>,
    kv_client: KvClient<Channel>,
    watch_client: WatchClient<Channel>,
    cluster_client: ClusterClient<Channel>,
    lease_client: LeaseClient<Channel>,
    token: Arc<RwLock<Option<MetadataValue<Ascii>>>>,
    auth_user: Option<(String, String)>,
}

impl AuthOp for Client {
    async fn authenticate<R>(&self, req: R) -> Result<AuthenticateResponse>
    where
        R: Into<AuthenticateRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self.auth_client.clone().authenticate(req).await?;

        Ok(resp.into_inner().into())
    }

    async fn auth_status(&self) -> Result<AuthStatusResponse> {
        let req = tonic::Request::new(AuthStatusRequest::default().into());
        let resp = match self.auth_user {
            Some(_) => {
                self.execute_with_retries(req, |req| async {
                    self.auth_client.clone().auth_status(req).await
                })
                .await?
            }
            None => self.auth_client.clone().auth_status(req).await?,
        };

        Ok(resp.into_inner().into())
    }

    async fn auth_enable(&self) -> Result<AuthEnableResponse> {
        let req = tonic::Request::new(AuthEnableRequest::default().into());
        let resp = match self.auth_user {
            Some(_) => {
                self.execute_with_retries(req, |req| async {
                    self.auth_client.clone().auth_enable(req).await
                })
                .await?
            }
            None => self.auth_client.clone().auth_enable(req).await?,
        };

        Ok(resp.into_inner().into())
    }

    async fn auth_disable(&self) -> Result<AuthDisableResponse> {
        let req = tonic::Request::new(AuthDisableRequest::default().into());
        let resp = match self.auth_user {
            Some(_) => {
                self.execute_with_retries(req, |req| async {
                    self.auth_client.clone().auth_disable(req).await
                })
                .await?
            }
            None => self.auth_client.clone().auth_disable(req).await?,
        };

        Ok(resp.into_inner().into())
    }

    async fn role_add<R>(&self, req: R) -> Result<AuthRoleAddResponse>
    where
        R: Into<AuthRoleAddRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = match self.auth_user {
            Some(_) => {
                self.execute_with_retries(req, |req| async {
                    self.auth_client.clone().role_add(req).await
                })
                .await?
            }
            None => self.auth_client.clone().role_add(req).await?,
        };

        Ok(resp.into_inner().into())
    }

    async fn role_delete<R>(&self, req: R) -> Result<AuthRoleDeleteResponse>
    where
        R: Into<AuthRoleDeleteRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = match self.auth_user {
            Some(_) => {
                self.execute_with_retries(req, |req| async {
                    self.auth_client.clone().role_delete(req).await
                })
                .await?
            }
            None => self.auth_client.clone().role_delete(req).await?,
        };

        Ok(resp.into_inner().into())
    }

    async fn role_list(&self) -> Result<AuthRoleListResponse> {
        let req = tonic::Request::new(AuthRoleListRequest::default().into());
        let resp = match self.auth_user {
            Some(_) => {
                self.execute_with_retries(req, |req| async {
                    self.auth_client.clone().role_list(req).await
                })
                .await?
            }
            None => self.auth_client.clone().role_list(req).await?,
        };

        Ok(resp.into_inner().into())
    }
}

impl Client {
    async fn new_channel(cfg: &ClientConfig) -> Result<Channel> {
        let mut endpoints = Vec::with_capacity(cfg.endpoints.len());
        for e in cfg.endpoints.iter() {
            #[cfg(not(feature = "tls"))]
            let c = Channel::from_shared(e.url.clone())?
                .connect_timeout(cfg.connect_timeout)
                .http2_keep_alive_interval(cfg.http2_keep_alive_interval);

            #[cfg(feature = "tls")]
            let mut c = Channel::from_shared(e.url.clone())?
                .connect_timeout(cfg.connect_timeout)
                .http2_keep_alive_interval(cfg.http2_keep_alive_interval);
            #[cfg(feature = "tls")]
            {
                if let Some(tls) = e.tls_opt.to_owned() {
                    c = c.tls_config(tls)?;
                }
            }

            endpoints.push(c);
        }

        Ok(Channel::balance_list(endpoints.into_iter()))
    }

    /// new connect to etcd cluster and returns a client.
    ///
    /// # Errors
    /// Will returns `Err` if failed to contact with given endpoints or authentication failed.
    pub async fn new(cfg: ClientConfig) -> Result<Self> {
        let channel = Self::new_channel(&cfg).await?;

        let auth_client = AuthClient::new(channel.clone());
        let kv_client = KvClient::new(channel.clone());
        let watch_client = WatchClient::new(channel.clone());
        let cluster_client = ClusterClient::new(channel.clone());
        let lease_client = LeaseClient::new(channel);

        let mut cli = Self {
            auth_client,
            kv_client,
            watch_client,
            cluster_client,
            lease_client,
            auth_user: None,
            token: Arc::new(RwLock::new(None)),
        };

        if let Some((username, password)) = cfg.auth {
            cli.auth_user = Some((username, password));
            cli.refresh_token().await.unwrap();
        };

        Ok(cli)
    }

    async fn refresh_token(&self) -> Result<()> {
        if let Some((username, password)) = &self.auth_user {
            let token = self.authenticate((username, password)).await?.token;
            let t = match MetadataValue::try_from(&token) {
                Ok(t) => t,
                Err(err) => return Err(Error::ParseMetadataToken(err.to_string())),
            };
            let mut x = self.token.write().await;
            *x = Some(t);
        }

        Ok(())
    }

    async fn set_token<T>(&self, req: &mut tonic::Request<T>) {
        let token = self.token.clone();
        let h = token.read().await;
        if let Some(token) = h.to_owned() {
            req.metadata_mut().insert("authorization", token);
        }
    }

    async fn execute_with_retries<F, Fut, T, R>(&self, req: tonic::Request<T>, f: F) -> Result<R>
    where
        F: Fn(tonic::Request<T>) -> Fut,
        Fut: Future<Output = std::result::Result<R, Status>>,
        T: Clone,
    {
        for _i in 1..=MAX_RETRY {
            let mut new_req = tonic::Request::new(req.get_ref().clone());
            self.set_token(&mut new_req).await;

            match f(new_req).await {
                Ok(response) => {
                    return Ok(response);
                }
                Err(status) => {
                    if status.code() == tonic::Code::Unauthenticated {
                        self.refresh_token().await?;
                    } else if status.code() == tonic::Code::Unavailable {
                        continue;
                    } else {
                        return Err(Error::Response(status));
                    }
                }
            }
        }
        Err(Error::ExecuteFailed)
    }
}

impl KeyValueOp for Client {
    async fn put<R>(&self, req: R) -> Result<PutResponse>
    where
        R: Into<PutRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async { self.kv_client.clone().put(req).await })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn get<R>(&self, req: R) -> Result<RangeResponse>
    where
        R: Into<RangeRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async { self.kv_client.clone().range(req).await })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn get_all(&self) -> Result<RangeResponse> {
        self.get(KeyRange::all()).await
    }

    async fn get_by_prefix<K>(&self, p: K) -> Result<RangeResponse>
    where
        K: Into<Vec<u8>>,
    {
        self.get(KeyRange::prefix(p)).await
    }

    async fn get_range<F, E>(&self, from: F, end: E) -> Result<RangeResponse>
    where
        F: Into<Vec<u8>>,
        E: Into<Vec<u8>>,
    {
        self.get(KeyRange::range(from, end)).await
    }

    async fn delete<R>(&self, req: R) -> Result<DeleteResponse>
    where
        R: Into<DeleteRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.kv_client.clone().delete_range(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn delete_all(&self) -> Result<DeleteResponse> {
        self.delete(KeyRange::all()).await
    }

    async fn delete_by_prefix<K>(&self, p: K) -> Result<DeleteResponse>
    where
        K: Into<Vec<u8>>,
    {
        self.delete(KeyRange::prefix(p)).await
    }

    async fn delete_range<F, E>(&self, from: F, end: E) -> Result<DeleteResponse>
    where
        F: Into<Vec<u8>>,
        E: Into<Vec<u8>>,
    {
        self.delete(KeyRange::range(from, end)).await
    }

    async fn txn<R>(&self, req: R) -> Result<TxnResponse>
    where
        R: Into<TxnRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async { self.kv_client.clone().txn(req).await })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn compact<R>(&self, req: R) -> Result<CompactResponse>
    where
        R: Into<CompactRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.kv_client.clone().compact(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }
}

impl WatchOp for Client {
    async fn watch<R>(&self, req: R) -> Result<(WatchStream, WatchCanceler)>
    where
        R: Into<WatchCreateRequest>,
    {
        let (tx, rx) = channel::<etcdserverpb::WatchRequest>(128);

        tx.send(req.into().into()).await?;

        let mut req = tonic::Request::new(ReceiverStream::new(rx));
        self.refresh_token().await?;
        self.set_token(&mut req).await;

        req.metadata_mut()
            .insert("hasleader", "true".try_into().unwrap());

        let resp = self.watch_client.clone().watch(req).await?;

        let mut inbound = resp.into_inner();

        let watch_id = match inbound.message().await? {
            Some(resp) => {
                if !resp.created {
                    return Err(Error::WatchEvent(
                        "should receive created event at first".to_owned(),
                    ));
                }
                if resp.canceled {
                    return Err(Error::WatchEvent(resp.cancel_reason));
                }
                assert!(resp.events.is_empty(), "received created event {:?}", resp);
                resp.watch_id
            }

            None => return Err(Error::CreateWatch),
        };

        Ok((WatchStream::new(inbound), WatchCanceler::new(watch_id, tx)))
    }
}

impl LeaseOp for Client {
    async fn grant_lease<R>(&self, req: R) -> Result<LeaseGrantResponse>
    where
        R: Into<LeaseGrantRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.lease_client.clone().lease_grant(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn revoke<R>(&self, req: R) -> Result<LeaseRevokeResponse>
    where
        R: Into<LeaseRevokeRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.lease_client.clone().lease_revoke(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn keep_alive_for(&self, lease_id: LeaseId) -> Result<LeaseKeepAlive> {
        let (req_tx, req_rx) = channel(1024);

        let req_rx = ReceiverStream::new(req_rx);

        let initial_req = LeaseKeepAliveRequest { id: lease_id };

        req_tx
            .send(initial_req)
            .await
            .map_err(|_| Error::ChannelClosed)?;

        let mut resp_rx = self
            .lease_client
            .clone()
            .lease_keep_alive(req_rx)
            .await?
            .into_inner();

        let lease_id = match resp_rx.message().await? {
            Some(resp) => resp.id,
            None => {
                return Err(Error::CreateWatch);
            }
        };

        Ok(LeaseKeepAlive::new(lease_id, req_tx, resp_rx))
    }

    async fn time_to_live<R>(&self, req: R) -> Result<LeaseTimeToLiveResponse>
    where
        R: Into<LeaseTimeToLiveRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.lease_client.clone().lease_time_to_live(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }
}

impl ClusterOp for Client {
    async fn member_add<R>(&self, req: R) -> Result<MemberAddResponse>
    where
        R: Into<MemberAddRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.cluster_client.clone().member_add(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn member_remove<R>(&self, req: R) -> Result<MemberRemoveResponse>
    where
        R: Into<MemberRemoveRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.cluster_client.clone().member_remove(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn member_update<R>(&self, req: R) -> Result<MemberUpdateResponse>
    where
        R: Into<MemberUpdateRequest>,
    {
        let req = tonic::Request::new(req.into().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.cluster_client.clone().member_update(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }

    async fn member_list(&self) -> Result<MemberListResponse> {
        let req = tonic::Request::new(MemberListRequest::new().into());
        let resp = self
            .execute_with_retries(req, |req| async {
                self.cluster_client.clone().member_list(req).await
            })
            .await?;

        Ok(resp.into_inner().into())
    }
}
