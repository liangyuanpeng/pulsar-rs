use crate::connection_manager::{BrokerAddress, ConnectionManager};
use crate::error::{ConnectionError, ServiceDiscoveryError};
use crate::executor::Executor;
use crate::message::proto::{
    command_lookup_topic_response, command_partitioned_topic_metadata_response,
    CommandLookupTopicResponse,
};
use futures::{future::try_join_all, FutureExt};
use std::{sync::Arc, time::Duration};
use url::Url;

/// Look up broker addresses for topics and partitioned topics
///
/// The ServiceDiscovery object provides a single interface to start
/// interacting with a cluster. It will automatically follow redirects
/// or use a proxy, and aggregate broker connections
#[derive(Clone)]
pub struct ServiceDiscovery<Exe: Executor + ?Sized> {
    manager: Arc<ConnectionManager<Exe>>,
}

impl<Exe: Executor> ServiceDiscovery<Exe> {
    pub fn with_manager(manager: Arc<ConnectionManager<Exe>>) -> Self {
        ServiceDiscovery { manager }
    }

    /// get the broker address for a topic
    pub async fn lookup_topic<S: Into<String>>(
        &self,
        topic: S,
    ) -> Result<BrokerAddress, ServiceDiscoveryError> {
        let topic = topic.into();
        let mut proxied_query = false;
        let mut conn = self.manager.get_base_connection().await?;
        let base_url = self.manager.url.clone();
        let mut is_authoritative = false;
        let mut broker_address = self.manager.get_base_address();

        let mut max_retries = 20u8;
        loop {
            let response = match conn
                .sender()
                .lookup_topic(topic.to_string(), is_authoritative)
                .await
            {
                Ok(res) => res,
                Err(ConnectionError::Disconnected) => {
                    error!("tried to lookup a topic but connection was closed, reconnecting...");
                    conn = self.manager.get_connection(&broker_address).await?;
                    conn.sender()
                        .lookup_topic(topic.to_string(), is_authoritative)
                        .await?
                }
                Err(e) => return Err(e.into()),
            };

            if response.response.is_none()
                || response.response
                    == Some(command_lookup_topic_response::LookupType::Failed as i32)
            {
                let error = response.error.and_then(crate::error::server_error);
                if error == Some(crate::message::proto::ServerError::ServiceNotReady)
                    && max_retries > 0
                {
                    error!("lookup({}) answered ServiceNotReady, retrying request after 500ms (max_retries = {})", topic, max_retries);
                    max_retries -= 1;
                    Exe::delay(Duration::from_millis(500)).await;
                    continue;
                }
                return Err(ServiceDiscoveryError::Query(
                    error,
                    response.message.clone(),
                ));
            }

            let LookupResponse {
                broker_url,
                broker_url_tls,
                proxy,
                redirect,
                authoritative,
            } = convert_lookup_response(&response)?;
            is_authoritative = authoritative;

            // use the TLS connection if available
            let connection_url = broker_url_tls.clone().unwrap_or_else(|| broker_url.clone());

            // if going through a proxy, we use the base URL
            let url = if proxied_query || proxy {
                base_url.clone()
            } else {
                connection_url.clone()
            };

            let broker_url = match broker_url_tls {
                Some(u) => format!("{}:{}", u.host_str().unwrap(), u.port().unwrap_or(6651)),
                None => format!(
                    "{}:{}",
                    broker_url.host_str().unwrap(),
                    broker_url.port().unwrap_or(6650)
                ),
            };

            broker_address = BrokerAddress {
                url,
                broker_url,
                proxy: proxied_query || proxy,
            };

            // if the response indicated a redirect, do another query
            // to the target broker
            if redirect {
                conn = self.manager.get_connection(&broker_address).await?;
                proxied_query = broker_address.proxy;
                continue;
            } else {
                let res = self
                    .manager
                    .get_connection(&broker_address)
                    .await
                    .map(|_| broker_address)
                    .map_err(ServiceDiscoveryError::Connection);
                break res;
            }
        }
    }

    /// get the number of partitions for a partitioned topic
    pub async fn lookup_partitioned_topic_number<S: Into<String>>(
        &self,
        topic: S,
    ) -> Result<u32, ServiceDiscoveryError> {
        let mut connection = self.manager.get_base_connection().await?;
        let topic = topic.into();

        let mut max_retries = 20u8;
        let response = loop {
            let response = match connection.sender().lookup_partitioned_topic(&topic).await {
                Ok(res) => res,
                Err(ConnectionError::Disconnected) => {
                    error!("tried to lookup a topic but connection was closed, reconnecting...");
                    connection = self.manager.get_base_connection().await?;
                    connection.sender().lookup_partitioned_topic(&topic).await?
                }
                Err(e) => return Err(e.into()),
            };

            if response.response.is_none()
                || response.response
                    == Some(command_partitioned_topic_metadata_response::LookupType::Failed as i32)
            {
                let error = response.error.and_then(crate::error::server_error);
                if error == Some(crate::message::proto::ServerError::ServiceNotReady)
                    && max_retries > 0
                {
                    error!("lookup_partitioned_topic_number({}) answered ServiceNotReady, retrying request after 500ms (max_retries = {})", topic, max_retries);
                    max_retries -= 1;
                    Exe::delay(Duration::from_millis(500)).await;
                    continue;
                }
                return Err(ServiceDiscoveryError::Query(
                    error,
                    response.message.clone(),
                ));
            }

            break response;
        };

        match response.partitions {
            Some(partitions) => Ok(partitions),
            None => Err(ServiceDiscoveryError::Query(
                response.error.and_then(crate::error::server_error),
                response.message,
            )),
        }
    }

    /// get the list of topic names and addresses for a partitioned topic
    pub async fn lookup_partitioned_topic<S: Into<String>>(
        &self,
        topic: S,
    ) -> Result<Vec<(String, BrokerAddress)>, ServiceDiscoveryError> {
        let topic = topic.into();
        let partitions = self.lookup_partitioned_topic_number(&topic).await?;
        let topics = (0..partitions)
            .map(|nb| {
                let t = format!("{}-partition-{}", topic, nb);
                self.lookup_topic(t.clone())
                    .map(move |address_res| match address_res {
                        Err(e) => Err(e),
                        Ok(address) => Ok((t, address)),
                    })
            })
            .collect::<Vec<_>>();
        try_join_all(topics).await
    }
}

struct LookupResponse {
    pub broker_url: Url,
    pub broker_url_tls: Option<Url>,
    pub proxy: bool,
    pub redirect: bool,
    pub authoritative: bool,
}

/// extracts information from a lookup response
fn convert_lookup_response(
    response: &CommandLookupTopicResponse,
) -> Result<LookupResponse, ServiceDiscoveryError> {
    let proxy = response.proxy_through_service_url.unwrap_or(false);
    let authoritative = response.authoritative.unwrap_or(false);
    let redirect =
        response.response == Some(command_lookup_topic_response::LookupType::Redirect as i32);

    if response.broker_service_url.is_none() {
        return Err(ServiceDiscoveryError::NotFound);
    }

    let broker_url = Url::parse(&response.broker_service_url.clone().unwrap()).map_err(|e| {
        error!("error parsing URL: {:?}", e);
        ServiceDiscoveryError::NotFound
    })?;

    let broker_url_tls = match response.broker_service_url_tls.as_ref() {
        Some(u) => Some(Url::parse(&u).map_err(|e| {
            error!("error parsing URL: {:?}", e);
            ServiceDiscoveryError::NotFound
        })?),
        None => None,
    };

    Ok(LookupResponse {
        broker_url,
        broker_url_tls,
        proxy,
        redirect,
        authoritative,
    })
}
