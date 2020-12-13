//! A `ProxyStream` that bypasses or proxies data through proxy server automatically

use std::{
    io::{self, IoSlice},
    net::SocketAddr,
    pin::Pin,
    sync::Arc,
    task::{self, Poll},
};

use pin_project::pin_project;
use shadowsocks::{
    context::SharedContext,
    net::{ConnectOpts, TcpStream},
    relay::{
        socks5::Address,
        tcprelay::proxy_stream::{ProxyClientStream, ProxyClientStreamReadHalf, ProxyClientStreamWriteHalf},
    },
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpStream as TokioTcpStream,
    },
};

use crate::{
    local::{acl::AccessControl, loadbalancing::ServerIdent},
    net::{FlowStat, MonProxyStream},
};

#[pin_project(project = AutoProxyClientStreamProj)]
pub enum AutoProxyClientStream {
    Proxied(#[pin] ProxyClientStream<MonProxyStream<TokioTcpStream>>),
    Bypassed(#[pin] TokioTcpStream),
}

impl AutoProxyClientStream {
    /// Connect to target `addr` via shadowsocks' server configured by `svr_cfg`
    pub async fn connect_with_opts_acl<A, E>(
        context: SharedContext,
        server: &ServerIdent<E>,
        addr: A,
        opts: &ConnectOpts,
        flow_stat: Arc<FlowStat>,
        acl: &AccessControl,
    ) -> io::Result<AutoProxyClientStream>
    where
        A: Into<Address>,
    {
        let addr = addr.into();
        if acl.check_target_bypassed(&context, &addr).await {
            // Connect directly.
            let stream = TcpStream::connect_remote_with_opts(&context, &addr, opts).await?;
            Ok(AutoProxyClientStream::Bypassed(stream.into()))
        } else {
            AutoProxyClientStream::connect_with_opts(context, server, addr, opts, flow_stat).await
        }
    }

    /// Connect to target `addr` via shadowsocks' server configured by `svr_cfg`
    pub async fn connect_with_opts<A, E>(
        context: SharedContext,
        server: &ServerIdent<E>,
        addr: A,
        opts: &ConnectOpts,
        flow_stat: Arc<FlowStat>,
    ) -> io::Result<AutoProxyClientStream>
    where
        A: Into<Address>,
    {
        let svr_cfg = server.server_config();
        let stream = match ProxyClientStream::connect_with_opts_map(context, svr_cfg, addr, opts, |stream| {
            MonProxyStream::from_stream(stream, flow_stat)
        })
        .await
        {
            Ok(s) => s,
            Err(err) => {
                server.report_failure().await;
                return Err(err);
            }
        };
        Ok(AutoProxyClientStream::Proxied(stream))
    }

    pub(crate) async fn connect_with_opts_acl_opt<A, E>(
        context: SharedContext,
        server: &ServerIdent<E>,
        addr: A,
        opts: &ConnectOpts,
        flow_stat: Arc<FlowStat>,
        acl: &Option<Arc<AccessControl>>,
    ) -> io::Result<AutoProxyClientStream>
    where
        A: Into<Address>,
    {
        match *acl {
            None => AutoProxyClientStream::connect_with_opts(context, server, addr, opts, flow_stat).await,
            Some(ref acl) => {
                AutoProxyClientStream::connect_with_opts_acl(context, server, addr, opts, flow_stat, acl).await
            }
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match *self {
            AutoProxyClientStream::Proxied(ref s) => s.get_ref().get_ref().local_addr(),
            AutoProxyClientStream::Bypassed(ref s) => s.local_addr(),
        }
    }

    pub fn set_nodelay(&self, nodelay: bool) -> io::Result<()> {
        match *self {
            AutoProxyClientStream::Proxied(ref s) => s.get_ref().get_ref().set_nodelay(nodelay),
            AutoProxyClientStream::Bypassed(ref s) => s.set_nodelay(nodelay),
        }
    }

    pub fn is_proxied(&self) -> bool {
        matches!(*self, AutoProxyClientStream::Proxied(..))
    }

    pub fn is_bypassed(&self) -> bool {
        matches!(*self, AutoProxyClientStream::Bypassed(..))
    }
}

impl AsyncRead for AutoProxyClientStream {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            AutoProxyClientStreamProj::Proxied(s) => s.poll_read(cx, buf),
            AutoProxyClientStreamProj::Bypassed(s) => s.poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for AutoProxyClientStream {
    fn poll_write(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.project() {
            AutoProxyClientStreamProj::Proxied(s) => s.poll_write(cx, buf),
            AutoProxyClientStreamProj::Bypassed(s) => s.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            AutoProxyClientStreamProj::Proxied(s) => s.poll_flush(cx),
            AutoProxyClientStreamProj::Bypassed(s) => s.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            AutoProxyClientStreamProj::Proxied(s) => s.poll_shutdown(cx),
            AutoProxyClientStreamProj::Bypassed(s) => s.poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.project() {
            AutoProxyClientStreamProj::Proxied(s) => s.poll_write_vectored(cx, bufs),
            AutoProxyClientStreamProj::Bypassed(s) => s.poll_write_vectored(cx, bufs),
        }
    }
}

impl From<ProxyClientStream<MonProxyStream<TokioTcpStream>>> for AutoProxyClientStream {
    fn from(s: ProxyClientStream<MonProxyStream<TokioTcpStream>>) -> Self {
        AutoProxyClientStream::Proxied(s)
    }
}

impl AutoProxyClientStream {
    pub fn into_split(self) -> (AutoProxyClientStreamReadHalf, AutoProxyClientStreamWriteHalf) {
        match self {
            AutoProxyClientStream::Proxied(s) => {
                let (r, w) = s.into_split();
                (
                    AutoProxyClientStreamReadHalf::Proxied(r),
                    AutoProxyClientStreamWriteHalf::Proxied(w),
                )
            }
            AutoProxyClientStream::Bypassed(s) => {
                let (r, w) = s.into_split();
                (
                    AutoProxyClientStreamReadHalf::Bypassed(r),
                    AutoProxyClientStreamWriteHalf::Bypassed(w),
                )
            }
        }
    }
}

#[pin_project(project = AutoProxyClientStreamReadHalfProj)]
pub enum AutoProxyClientStreamReadHalf {
    Proxied(#[pin] ProxyClientStreamReadHalf<MonProxyStream<TokioTcpStream>>),
    Bypassed(#[pin] OwnedReadHalf),
}

impl AsyncRead for AutoProxyClientStreamReadHalf {
    fn poll_read(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            AutoProxyClientStreamReadHalfProj::Proxied(s) => s.poll_read(cx, buf),
            AutoProxyClientStreamReadHalfProj::Bypassed(s) => s.poll_read(cx, buf),
        }
    }
}

#[pin_project(project = AutoProxyClientStreamWriteHalfProj)]
pub enum AutoProxyClientStreamWriteHalf {
    Proxied(#[pin] ProxyClientStreamWriteHalf<MonProxyStream<TokioTcpStream>>),
    Bypassed(#[pin] OwnedWriteHalf),
}

impl AsyncWrite for AutoProxyClientStreamWriteHalf {
    fn poll_write(self: Pin<&mut Self>, cx: &mut task::Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        match self.project() {
            AutoProxyClientStreamWriteHalfProj::Proxied(s) => s.poll_write(cx, buf),
            AutoProxyClientStreamWriteHalfProj::Bypassed(s) => s.poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            AutoProxyClientStreamWriteHalfProj::Proxied(s) => s.poll_flush(cx),
            AutoProxyClientStreamWriteHalfProj::Bypassed(s) => s.poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        match self.project() {
            AutoProxyClientStreamWriteHalfProj::Proxied(s) => s.poll_shutdown(cx),
            AutoProxyClientStreamWriteHalfProj::Bypassed(s) => s.poll_shutdown(cx),
        }
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        match self.project() {
            AutoProxyClientStreamWriteHalfProj::Proxied(s) => s.poll_write_vectored(cx, bufs),
            AutoProxyClientStreamWriteHalfProj::Bypassed(s) => s.poll_write_vectored(cx, bufs),
        }
    }
}