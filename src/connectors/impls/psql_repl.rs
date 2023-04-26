use std::time::Duration;
use std::str::FromStr;
// Copyright 2021, The Tremor Team
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
use crate::connectors::prelude::*;
use tremor_common::time::nanotime;

use mz_postgres_util::{Config as MzConfig};
use tokio_postgres::config::Config as TokioPgConfig;
mod postgres_replication;

#[derive(Deserialize, Debug, Clone)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Interval in nanoseconds
    pub interval: u64,
    /// Host name
    pub host: String,
    /// Port number
    pub port: u16,
    /// Username
    pub username: String,
    /// Password string
    pub password: String,
    /// Database name
    pub dbname: String,
}

impl ConfigImpl for Config {}

#[derive(Debug, Default)]
pub(crate) struct Builder {}

#[async_trait::async_trait]
impl ConnectorBuilder for Builder {
    fn connector_type(&self) -> ConnectorType {
        "psql_repl".into()
    }

    async fn build_cfg(
        &self,
        _: &Alias,
        _: &ConnectorConfig,
        raw: &Value,
        _kill_switch: &KillSwitch,
    ) -> Result<Box<dyn Connector>> {
        let config = Config::new(raw)?;
        let origin_uri = EventOriginUri {
            scheme: "tremor-psql-repl".to_string(),
            host: config.host,
            port: Option::from(config.port),
            path: vec![config.interval.to_string()],
        };
        let database = config.dbname;
        let username = config.username;
        let password = config.password;
        let pg_config= TokioPgConfig::from_str(&format!("host={} port=5432 user={} password={} dbname={}", origin_uri.host, username, password, database))?;
        let connection_config = MzConfig::new(pg_config, mz_postgres_util::TunnelConfig::Direct)?;

        Ok(Box::new(PostgresReplication {
            interval: config.interval,
            connection_config,
            origin_uri,
        }))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct PostgresReplication {
    interval: u64,
    connection_config : MzConfig,
    origin_uri: EventOriginUri,
}

#[async_trait::async_trait()]
impl Connector for PostgresReplication {
    async fn create_source(
        &mut self,
        ctx: SourceContext,
        builder: SourceManagerBuilder,
    ) -> Result<Option<SourceAddr>> {
        let source = PostgresReplicationSource::new(self.interval, self.connection_config.clone(), self.origin_uri.clone());
        Ok(Some(builder.spawn(source, ctx)))
    }

    fn codec_requirements(&self) -> CodecReq {
        CodecReq::Structured
    }
}

struct PostgresReplicationSource {
    interval_ns: u64,
    next: u64,
    connection_config : MzConfig,
    origin_uri: EventOriginUri,
    id: u64,
}

impl PostgresReplicationSource {
    fn new(interval_ns: u64, connection_config: MzConfig, origin_uri: EventOriginUri) -> Self {
        Self {
            interval_ns,
            connection_config,
            next: nanotime() + interval_ns, // dummy placeholer
            origin_uri,
            id: 0,
        }
    }
}

#[async_trait::async_trait()]
impl Source for PostgresReplicationSource {
    async fn connect(&mut self, _ctx: &SourceContext, _attempt: &Attempt) -> Result<bool> {
        self.next = nanotime() + self.interval_ns;
        let _client =
            self.connection_config.connect("postgres_connection").await?;
        Ok(true)
    }

    async fn pull_data(&mut self, pull_id: &mut u64, _ctx: &SourceContext) -> Result<SourceReply> {
        let now = nanotime();
        // we need to wait here before we continue to fulfill the interval conditions
        if now < self.next {
            tokio::time::sleep(Duration::from_nanos(self.next - now)).await;
        }
        self.next += self.interval_ns;
        *pull_id = self.id;
        self.id += 1;

        postgres_replication::replication(self.connection_config.clone()).await?;

        let data = literal!({
            "connector": "psql-repl",
            "ingest_ns": now,
            "id": *pull_id
        });
        Ok(SourceReply::Structured {
            origin_uri: self.origin_uri.clone(),
            payload: data.into(),
            stream: DEFAULT_STREAM_ID,
            port: None,
        })
    }
    fn is_transactional(&self) -> bool {
        false
    }

    fn asynchronous(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {

    use crate::{config::Reconnect, connectors::prelude::*};

    #[tokio::test(flavor = "multi_thread")]
    async fn missing_config() -> Result<()> {
        let alias = Alias::new("flow", "connector");
        let builder = super::Builder::default();
        let connector_config = super::ConnectorConfig {
            connector_type: builder.connector_type(),
            codec: None,
            config: None,
            preprocessors: None,
            postprocessors: None,
            reconnect: Reconnect::None,
            metrics_interval_s: Some(5),
        };
        let kill_switch = KillSwitch::dummy();
        assert!(matches!(
            builder
                .build(&alias, &connector_config, &kill_switch)
                .await
                .err(),
            Some(Error(ErrorKind::MissingConfiguration(_), _))
        ));
        Ok(())
    }
}
