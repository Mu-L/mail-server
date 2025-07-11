/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

use std::{fmt::Display, net::SocketAddr, sync::Arc, time::Duration};

use ahash::AHashMap;
use serde::{Deserialize, Serialize};
use tokio::net::TcpSocket;
use utils::{config::ipmask::IpAddrMask, snowflake::SnowflakeIdGenerator};

use crate::listener::TcpAcceptor;

pub mod listener;
pub mod tls;

#[derive(Default)]
pub struct Listeners {
    pub servers: Vec<Listener>,
    pub tcp_acceptors: AHashMap<String, TcpAcceptor>,
    pub span_id_gen: Arc<SnowflakeIdGenerator>,
}

#[derive(Debug, Default)]
pub struct Listener {
    pub id: String,
    pub protocol: ServerProtocol,
    pub listeners: Vec<TcpListener>,
    pub proxy_networks: Vec<IpAddrMask>,
    pub max_connections: u64,
    pub span_id_gen: Arc<SnowflakeIdGenerator>,
}

#[derive(Debug)]
pub struct TcpListener {
    pub socket: TcpSocket,
    pub addr: SocketAddr,
    pub backlog: Option<u32>,

    // TCP options
    pub ttl: Option<u32>,
    pub linger: Option<Duration>,
    pub nodelay: bool,
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy, Default, Serialize, Deserialize)]
pub enum ServerProtocol {
    #[default]
    Smtp,
    Lmtp,
    Imap,
    Pop3,
    Http,
    ManageSieve,
}

impl ServerProtocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServerProtocol::Smtp => "smtp",
            ServerProtocol::Lmtp => "lmtp",
            ServerProtocol::Imap => "imap",
            ServerProtocol::Http => "http",
            ServerProtocol::Pop3 => "pop3",
            ServerProtocol::ManageSieve => "managesieve",
        }
    }
}

impl Display for ServerProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
