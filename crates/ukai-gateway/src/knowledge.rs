//! Knowledge, as the gateway serves it: `ukai.gateway.v1.Knowledge` to its
//! clients and `WorkspaceKnowledge` to its agent pods (gRPC and gRPC-Web, on
//! its own port), each answered by asking `ukai-knowledge`
//! (`ukai.knowledge.v1.KnowledgeStore`) on behalf of whoever is asking.
//!
//! The gateway is what can tell who that is: a person by the gateway's
//! token, whose scope is their agent's in `gateway.toml`, or an agent pod by
//! its relay's token, whose scope and workspace its Job says. It names that
//! [`Caller`] to `ukai-knowledge`, which believes it and is reached with the
//! token the two share. What the service answers goes back as it is.

use std::sync::Arc;
use std::time::Duration;

use axum::http::HeaderMap;
use tonic::metadata::{Ascii, MetadataValue};
use tonic::transport::{Channel, Endpoint};
use tonic::{Code, Request, Response, Status};
use ukai_core::knowledge::Editor;
use ukai_proto::gateway::v1 as gw;
use ukai_proto::knowledge::v1 as pb;
use ukai_proto::knowledge::v1::knowledge_store_client::KnowledgeStoreClient;

use crate::Gateway;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const TIMEOUT: Duration = Duration::from_secs(20);

/// Whose knowledge a request is about, and who is asking.
#[derive(Debug, Clone)]
pub struct Caller {
    pub scope: String,
    pub editor: Editor,
    /// The agent pod's workspace, for a pod.
    pub workspace: Option<String>,
}

impl From<&Caller> for pb::Caller {
    fn from(caller: &Caller) -> Self {
        Self {
            scope: caller.scope.clone(),
            editor: pb::Editor::from(caller.editor).into(),
            workspace: caller.workspace.clone(),
        }
    }
}

/// `ukai-knowledge`, at the address the gateway was given.
#[derive(Clone)]
pub struct KnowledgeClient {
    client: KnowledgeStoreClient<Channel>,
    /// `Bearer <token>`, marked sensitive so it is never logged.
    token: MetadataValue<Ascii>,
}

impl KnowledgeClient {
    /// A client of the service at `url` (`http://ukai-knowledge:50051`),
    /// connecting on first use, so the gateway starts without it.
    pub fn new(url: &str, token: &str) -> Result<Self, String> {
        let channel = Endpoint::from_shared(url.to_owned())
            .map_err(|error| format!("{url}: {error}"))?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(TIMEOUT)
            .connect_lazy();
        let mut token: MetadataValue<Ascii> = format!("Bearer {token}")
            .parse()
            .map_err(|_| "the knowledge token is not a valid header value".to_owned())?;
        token.set_sensitive(true);
        Ok(Self {
            client: KnowledgeStoreClient::new(channel),
            token,
        })
    }

    fn request<T>(&self, message: T) -> Request<T> {
        let mut request = Request::new(message);
        request
            .metadata_mut()
            .insert("authorization", self.token.clone());
        request
    }

    /// What the service said to the gateway, as the gateway's caller should
    /// hear it: the service refusing the gateway's own token is the
    /// gateway's failure, not the caller's.
    fn answer<T>(result: Result<Response<T>, Status>) -> Result<T, Status> {
        match result {
            Ok(response) => Ok(response.into_inner()),
            Err(status) if status.code() == Code::Unauthenticated => {
                tracing::warn!("ukai-knowledge does not take the gateway's UKAI_KNOWLEDGE_TOKEN");
                Err(Status::internal(
                    "the gateway cannot authenticate to its knowledge",
                ))
            }
            Err(status) => Err(status),
        }
    }

    async fn list(&self, caller: &Caller) -> Result<pb::ListResponse, Status> {
        let request = self.request(pb::ListRequest {
            caller: Some(caller.into()),
        });
        Self::answer(self.client.clone().list(request).await)
    }

    async fn get(&self, caller: &Caller, name: String) -> Result<pb::GetResponse, Status> {
        let request = self.request(pb::GetRequest {
            caller: Some(caller.into()),
            name,
        });
        Self::answer(self.client.clone().get(request).await)
    }

    async fn revisions(
        &self,
        caller: &Caller,
        name: String,
    ) -> Result<pb::RevisionsResponse, Status> {
        let request = self.request(pb::RevisionsRequest {
            caller: Some(caller.into()),
            name,
        });
        Self::answer(self.client.clone().revisions(request).await)
    }

    async fn put(
        &self,
        caller: &Caller,
        name: String,
        spec: Option<pb::NoteSpec>,
        base_revision: u64,
        commit: Option<String>,
    ) -> Result<pb::ChangeResponse, Status> {
        let request = self.request(pb::PutRequest {
            caller: Some(caller.into()),
            name,
            spec,
            base_revision,
            commit,
        });
        Self::answer(self.client.clone().put(request).await)
    }

    async fn delete(
        &self,
        caller: &Caller,
        name: String,
        revision: u64,
    ) -> Result<pb::ChangeResponse, Status> {
        let request = self.request(pb::DeleteRequest {
            caller: Some(caller.into()),
            name,
            revision,
        });
        Self::answer(self.client.clone().delete(request).await)
    }

    async fn review(
        &self,
        caller: &Caller,
        name: String,
        revision: u64,
        approve: bool,
    ) -> Result<pb::ChangeResponse, Status> {
        let request = self.request(pb::ReviewRequest {
            caller: Some(caller.into()),
            name,
            revision,
        });
        let mut client = self.client.clone();
        Self::answer(if approve {
            client.approve(request).await
        } else {
            client.reject(request).await
        })
    }
}

/// `ukai-knowledge`, or why there is none to ask.
fn store(gateway: &Gateway) -> Result<&KnowledgeClient, Status> {
    gateway.knowledge.as_ref().ok_or_else(|| {
        Status::unavailable(
            "this gateway keeps no knowledge: set UKAI_KNOWLEDGE_URL and UKAI_KNOWLEDGE_TOKEN",
        )
    })
}

/// The bearer token of a request.
fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty())
}

/// `ukai.gateway.v1.Knowledge`: a person, through one of the gateway's
/// agents.
pub struct ClientKnowledge {
    pub gateway: Arc<Gateway>,
}

impl ClientKnowledge {
    /// A person asking through `agent`, and the store, or why they may not.
    fn caller<T>(
        &self,
        request: &Request<T>,
        agent: &str,
    ) -> Result<(Caller, &KnowledgeClient), Status> {
        let headers = request.metadata().clone().into_headers();
        crate::access::authorize(&self.gateway, &headers)?;
        let caller = Caller {
            scope: crate::access::agent(&self.gateway, agent)?.knowledge_scope(),
            editor: Editor::User,
            workspace: None,
        };
        Ok((caller, store(&self.gateway)?))
    }
}

#[tonic::async_trait]
impl gw::knowledge_server::Knowledge for ClientKnowledge {
    async fn list(
        &self,
        request: Request<gw::AgentRequest>,
    ) -> Result<Response<gw::Listing>, Status> {
        let (caller, store) = self.caller(&request, &request.get_ref().agent)?;
        let notes = store.list(&caller).await?.notes;
        Ok(Response::new(gw::Listing {
            scope: caller.scope,
            notes,
        }))
    }

    async fn get(
        &self,
        request: Request<gw::NoteRequest>,
    ) -> Result<Response<pb::GetResponse>, Status> {
        let (caller, store) = self.caller(&request, &request.get_ref().agent)?;
        let name = request.into_inner().name;
        store.get(&caller, name).await.map(Response::new)
    }

    async fn revisions(
        &self,
        request: Request<gw::NoteRequest>,
    ) -> Result<Response<pb::RevisionsResponse>, Status> {
        let (caller, store) = self.caller(&request, &request.get_ref().agent)?;
        let name = request.into_inner().name;
        store.revisions(&caller, name).await.map(Response::new)
    }

    async fn put(
        &self,
        request: Request<gw::PutRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let (caller, store) = self.caller(&request, &request.get_ref().agent)?;
        let request = request.into_inner();
        store
            .put(
                &caller,
                request.name,
                request.spec,
                request.base_revision,
                None,
            )
            .await
            .map(Response::new)
    }

    async fn delete(
        &self,
        request: Request<gw::RevisionRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let (caller, store) = self.caller(&request, &request.get_ref().agent)?;
        let request = request.into_inner();
        store
            .delete(&caller, request.name, request.revision)
            .await
            .map(Response::new)
    }

    async fn approve(
        &self,
        request: Request<gw::RevisionRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let (caller, store) = self.caller(&request, &request.get_ref().agent)?;
        let request = request.into_inner();
        store
            .review(&caller, request.name, request.revision, true)
            .await
            .map(Response::new)
    }

    async fn reject(
        &self,
        request: Request<gw::RevisionRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let (caller, store) = self.caller(&request, &request.get_ref().agent)?;
        let request = request.into_inner();
        store
            .review(&caller, request.name, request.revision, false)
            .await
            .map(Response::new)
    }
}

/// `ukai.gateway.v1.WorkspaceKnowledge`: an agent pod, known by its relay's
/// token. There is no review here.
pub struct PodKnowledge {
    pub gateway: Arc<Gateway>,
}

impl PodKnowledge {
    /// The pod whose relay token the request carries, and the store, or why
    /// there is none. The scope and workspace are the pod's Job's, never the
    /// pod's say.
    async fn caller<T>(&self, request: &Request<T>) -> Result<(Caller, &KnowledgeClient), Status> {
        let headers = request.metadata().clone().into_headers();
        let token = bearer(&headers)
            .ok_or_else(|| Status::unauthenticated("a pod asks with its relay's token"))?;
        let pod = match self.gateway.cluster.pod_of(token).await {
            Ok(Some(pod)) => pod,
            Ok(None) => return Err(Status::unauthenticated("no agent pod has this token")),
            Err(error) => {
                tracing::warn!(%error, "cannot look up the pod asking for knowledge");
                return Err(Status::unavailable("cannot look up the pod asking"));
            }
        };
        let caller = Caller {
            scope: pod.scope,
            editor: Editor::Agent,
            workspace: Some(pod.workspace),
        };
        Ok((caller, store(&self.gateway)?))
    }
}

#[tonic::async_trait]
impl gw::workspace_knowledge_server::WorkspaceKnowledge for PodKnowledge {
    async fn list(
        &self,
        request: Request<gw::WorkspaceListRequest>,
    ) -> Result<Response<gw::Listing>, Status> {
        let (caller, store) = self.caller(&request).await?;
        let notes = store.list(&caller).await?.notes;
        Ok(Response::new(gw::Listing {
            scope: caller.scope,
            notes,
        }))
    }

    async fn get(
        &self,
        request: Request<gw::WorkspaceNoteRequest>,
    ) -> Result<Response<pb::GetResponse>, Status> {
        let (caller, store) = self.caller(&request).await?;
        let name = request.into_inner().name;
        store.get(&caller, name).await.map(Response::new)
    }

    async fn revisions(
        &self,
        request: Request<gw::WorkspaceNoteRequest>,
    ) -> Result<Response<pb::RevisionsResponse>, Status> {
        let (caller, store) = self.caller(&request).await?;
        let name = request.into_inner().name;
        store.revisions(&caller, name).await.map(Response::new)
    }

    async fn put(
        &self,
        request: Request<gw::WorkspacePutRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let (caller, store) = self.caller(&request).await?;
        let request = request.into_inner();
        store
            .put(
                &caller,
                request.name,
                request.spec,
                request.base_revision,
                request.commit,
            )
            .await
            .map(Response::new)
    }

    async fn delete(
        &self,
        request: Request<gw::WorkspaceRevisionRequest>,
    ) -> Result<Response<pb::ChangeResponse>, Status> {
        let (caller, store) = self.caller(&request).await?;
        let request = request.into_inner();
        store
            .delete(&caller, request.name, request.revision)
            .await
            .map(Response::new)
    }
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::*;

    #[rstest]
    #[case::bearer(Some("Bearer t0k"), Some("t0k"))]
    #[case::empty(Some("Bearer "), None)]
    #[case::basic(Some("Basic dTpw"), None)]
    #[case::none(None, None)]
    fn reads_the_bearer_token(#[case] header: Option<&str>, #[case] expected: Option<&str>) {
        let mut headers = HeaderMap::new();
        if let Some(header) = header {
            headers.insert(
                axum::http::header::AUTHORIZATION,
                header.parse().unwrap_or_else(|e| panic!("{e}")),
            );
        }
        assert_eq!(bearer(&headers), expected);
    }

    #[tokio::test]
    async fn the_services_refusal_of_the_gateway_is_the_gateways_failure() {
        let refused = KnowledgeClient::answer::<()>(Err(Status::unauthenticated("no")));
        assert_eq!(refused.map_err(|s| s.code()), Err(Code::Internal));
        let missing = KnowledgeClient::answer::<()>(Err(Status::not_found("no")));
        assert_eq!(missing.map_err(|s| s.code()), Err(Code::NotFound));
    }

    #[tokio::test]
    async fn the_token_is_never_shown() {
        let client = KnowledgeClient::new("http://ukai-knowledge:50051", "s3cret")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(!format!("{:?}", client.token).contains("s3cret"));
    }
}
