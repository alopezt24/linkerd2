use futures::prelude::*;
use kube::{
    runtime::wait::{await_condition, conditions},
    ResourceExt,
};
use linkerd2_proxy_api::{
    self as grpc, inbound::inbound_server_policies_client::InboundServerPoliciesClient,
};
use linkerd_policy_controller_k8s_api as k8s;
use linkerd_policy_test::with_temp_ns;
use tokio::{io, time};

#[tokio::test(flavor = "current_thread")]
async fn watch_updates() {
    with_temp_ns(|client, ns| async move {
        // Create a pod that does nothing. It's injected with a proxy, so we can attach resources to
        let pod_api = kube::Api::<k8s::Pod>::namespaced(client.clone(), &ns);
        let pod = pod_api
            .create(&kube::api::PostParams::default(), &mk_pause(&ns, "pause"))
            .await
            .expect("pod must apply");
        time::timeout(
            time::Duration::from_secs(60),
            await_condition(pod_api, &pod.name(), conditions::is_pod_running()),
        )
        .await
        .expect("pod failed to become running")
        .expect("pod failed to become running");
        tracing::trace!(?pod);

        // Start a watch
        let mut policy_api = policy_client(&client).await;
        let mut rx = policy_api
            .watch_port(tonic::Request::new(grpc::inbound::PortSpec {
                port: 4191,
                workload: format!("{}:{}", ns, pod.name()),
            }))
            .await
            .expect("failed to establish watch")
            .into_inner();

        let config = rx
            .next()
            .await
            .expect("watch must return an initial config")
            .expect("watch must return an initial config");
        tracing::trace!(?config);
        assert_eq!(
            config.protocol,
            Some(grpc::inbound::ProxyProtocol {
                kind: Some(grpc::inbound::proxy_protocol::Kind::Detect(
                    grpc::inbound::proxy_protocol::Detect {
                        timeout: Some(time::Duration::from_secs(10).into()),
                    }
                )),
            }),
        );
        assert_eq!(
            config.authorizations,
            vec![grpc::inbound::Authz {
                labels: Some((
                    "name".to_string(),
                    "default:all-unauthenticated".to_string()
                ))
                .into_iter()
                .collect(),
                authentication: Some(grpc::inbound::Authn {
                    permit: Some(grpc::inbound::authn::Permit::Unauthenticated(
                        grpc::inbound::authn::PermitUnauthenticated {}
                    )),
                }),
                networks: vec![
                    grpc::inbound::Network {
                        net: Some(grpc::net::IpNetwork::from(ipnet::IpNet::from(
                            ipnet::Ipv4Net::default()
                        ))),
                        except: vec![],
                    },
                    grpc::inbound::Network {
                        net: Some(grpc::net::IpNetwork::from(ipnet::IpNet::from(
                            ipnet::Ipv6Net::default()
                        ))),
                        except: vec![],
                    }
                ]
            }]
        );
        assert_eq!(
            config.labels,
            Some((
                "name".to_string(),
                "default:all-unauthenticated".to_string()
            ))
            .into_iter()
            .collect()
        );

        // Create a server that selects the pod's proxy admin server.
        let server = kube::Api::<k8s::policy::Server>::namespaced(client.clone(), &ns)
            .create(
                &kube::api::PostParams::default(),
                &mk_admin_server(&ns, "linkerd-admin"),
            )
            .await
            .expect("server must apply");
        tracing::trace!(?server);

        let config = rx
            .next()
            .await
            .expect("watch must return an initial config")
            .expect("watch must return an initial config");
        tracing::trace!(?config);
        assert_eq!(
            config.protocol,
            Some(grpc::inbound::ProxyProtocol {
                kind: Some(grpc::inbound::proxy_protocol::Kind::Http1(
                    grpc::inbound::proxy_protocol::Http1::default()
                )),
            }),
        );
        assert_eq!(config.authorizations, vec![]);
        assert_eq!(
            config.labels,
            Some(("name".to_string(), "linkerd-admin".to_string()))
                .into_iter()
                .collect()
        );

        // Create a server that selects the pod's proxy admin server.
        let server_authz =
            kube::Api::<k8s::policy::ServerAuthorization>::namespaced(client.clone(), &ns)
                .create(
                    &kube::api::PostParams::default(),
                    &k8s::policy::ServerAuthorization {
                        metadata: kube::api::ObjectMeta {
                            namespace: Some(ns.clone()),
                            name: Some("all-admin".to_string()),
                            ..Default::default()
                        },
                        spec: k8s::policy::ServerAuthorizationSpec {
                            server: k8s::policy::server_authorization::Server {
                                name: Some("linkerd-admin".to_string()),
                                selector: None,
                            },
                            client: k8s::policy::server_authorization::Client {
                                unauthenticated: true,
                                ..k8s::policy::server_authorization::Client::default()
                            },
                        },
                    },
                )
                .await
                .expect("serverauthorization must apply");
        tracing::trace!(?server_authz);

        let config = rx
            .next()
            .await
            .expect("watch must return an initial config")
            .expect("watch must return an initial config");
        tracing::trace!(?config);
        assert_eq!(
            config.protocol,
            Some(grpc::inbound::ProxyProtocol {
                kind: Some(grpc::inbound::proxy_protocol::Kind::Http1(
                    grpc::inbound::proxy_protocol::Http1::default()
                )),
            }),
        );
        assert_eq!(
            config.authorizations.first().unwrap().labels,
            Some(("name".to_string(), "all-admin".to_string()))
                .into_iter()
                .collect()
        );
        assert_eq!(
            *config
                .authorizations
                .first()
                .unwrap()
                .authentication
                .as_ref()
                .unwrap(),
            grpc::inbound::Authn {
                permit: Some(grpc::inbound::authn::Permit::Unauthenticated(
                    grpc::inbound::authn::PermitUnauthenticated {}
                )),
            }
        );

        assert_eq!(
            config.labels,
            Some(("name".to_string(), "linkerd-admin".to_string()))
                .into_iter()
                .collect()
        );
    })
    .await;
}

struct GrpcClient {
    tx: hyper::client::conn::SendRequest<tonic::body::BoxBody>,
}

async fn policy_client(client: &kube::Client) -> InboundServerPoliciesClient<GrpcClient> {
    let io = connect_to_api(client).await;
    let (tx, conn) = hyper::client::conn::Builder::new()
        .http2_only(true)
        .handshake(io)
        .await
        .expect("http connection failed");
    tokio::spawn(conn);
    InboundServerPoliciesClient::new(GrpcClient { tx })
}

async fn connect_to_api(client: &kube::Client) -> impl io::AsyncRead + io::AsyncWrite + Unpin {
    let pod = get_policy_controller_pod(client).await;
    let mut pf = kube::Api::<k8s::Pod>::namespaced(client.clone(), "linkerd")
        .portforward(&pod, &[8090])
        .await
        .expect("portforward failed");
    pf.ports()[0].stream().expect("must have a stream")
}

async fn get_policy_controller_pod(client: &kube::Client) -> String {
    kube::Api::<k8s::Pod>::namespaced(client.clone(), "linkerd")
        .list(
            &kube::api::ListParams::default()
                .labels("linkerd.io/control-plane-component=destination"),
        )
        .await
        .expect("must list controllers")
        .items
        .pop()
        .expect("must have a pod")
        .name()
}

fn mk_pause(ns: &str, name: &str) -> k8s::Pod {
    k8s::Pod {
        metadata: k8s::ObjectMeta {
            namespace: Some(ns.to_string()),
            name: Some(name.to_string()),
            annotations: Some(
                Some(("linkerd.io/inject".to_string(), "enabled".to_string()))
                    .into_iter()
                    .collect(),
            ),
            ..Default::default()
        },
        spec: Some(k8s::PodSpec {
            containers: vec![k8s::api::core::v1::Container {
                name: "pause".to_string(),
                image: Some("gcr.io/google_containers/pause:3.2".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..k8s::Pod::default()
    }
}

fn mk_admin_server(ns: &str, name: &str) -> k8s::policy::Server {
    k8s::policy::Server {
        metadata: k8s::ObjectMeta {
            namespace: Some(ns.to_string()),
            name: Some(name.to_string()),
            ..Default::default()
        },
        spec: k8s::policy::ServerSpec {
            pod_selector: k8s::labels::Selector::default(),
            port: k8s::policy::server::Port::Number(4191),
            proxy_protocol: Some(k8s::policy::server::ProxyProtocol::Http1),
        },
    }
}

impl hyper::service::Service<hyper::Request<tonic::body::BoxBody>> for GrpcClient {
    type Response = hyper::Response<hyper::Body>;
    type Error = hyper::Error;
    type Future = hyper::client::conn::ResponseFuture;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.tx.poll_ready(cx)
    }

    fn call(&mut self, req: hyper::Request<tonic::body::BoxBody>) -> Self::Future {
        let (mut parts, body) = req.into_parts();

        let mut uri = parts.uri.into_parts();
        uri.scheme = Some(hyper::http::uri::Scheme::HTTP);
        uri.authority = Some(
            "linkerd-destination.linkerd.svc.cluster.local:8090"
                .parse()
                .unwrap(),
        );
        parts.uri = hyper::Uri::from_parts(uri).unwrap();

        self.tx.call(hyper::Request::from_parts(parts, body))
    }
}
