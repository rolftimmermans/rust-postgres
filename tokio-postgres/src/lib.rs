extern crate antidote;
extern crate bytes;
extern crate fallible_iterator;
extern crate futures_cpupool;
extern crate postgres_protocol;
extern crate postgres_shared;
extern crate tokio_codec;
extern crate tokio_io;
extern crate tokio_tcp;
extern crate tokio_timer;

#[macro_use]
extern crate futures;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
#[macro_use]
extern crate state_machine_future;

#[cfg(all(unix, feature="uds"))]
extern crate tokio_uds;

use bytes::Bytes;
use futures::{Async, Future, Poll, Stream};
use postgres_shared::rows::RowIndex;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};

#[doc(inline)]
pub use postgres_shared::stmt::Column;
#[doc(inline)]
pub use postgres_shared::{error, params, types};
#[doc(inline)]
pub use postgres_shared::{CancelData, Notification};

use error::{DbError, Error};
use params::ConnectParams;
use tls::TlsConnect;
use types::{FromSql, ToSql, Type};

mod proto;
pub mod tls;

static NEXT_STATEMENT_ID: AtomicUsize = AtomicUsize::new(0);

fn next_statement() -> String {
    format!("s{}", NEXT_STATEMENT_ID.fetch_add(1, Ordering::SeqCst))
}

fn bad_response() -> Error {
    Error::from(io::Error::new(
        io::ErrorKind::InvalidInput,
        "the server returned an unexpected response",
    ))
}

fn disconnected() -> Error {
    Error::from(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "server disconnected",
    ))
}

pub enum TlsMode {
    None,
    Prefer(Box<TlsConnect>),
    Require(Box<TlsConnect>),
}

pub fn cancel_query(params: ConnectParams, tls: TlsMode, cancel_data: CancelData) -> CancelQuery {
    CancelQuery(proto::CancelFuture::new(params, tls, cancel_data))
}

pub fn connect(params: ConnectParams, tls: TlsMode) -> Handshake {
    Handshake(proto::HandshakeFuture::new(params, tls))
}

pub struct Client(proto::Client);

impl Client {
    pub fn prepare(&mut self, query: &str) -> Prepare {
        self.prepare_typed(query, &[])
    }

    pub fn prepare_typed(&mut self, query: &str, param_types: &[Type]) -> Prepare {
        Prepare(self.0.prepare(next_statement(), query, param_types))
    }

    pub fn execute(&mut self, statement: &Statement, params: &[&ToSql]) -> Execute {
        Execute(self.0.execute(&statement.0, params))
    }

    pub fn query(&mut self, statement: &Statement, params: &[&ToSql]) -> Query {
        Query(self.0.query(&statement.0, params))
    }

    pub fn copy_out(&mut self, statement: &Statement, params: &[&ToSql]) -> CopyOut {
        CopyOut(self.0.copy_out(&statement.0, params))
    }

    pub fn transaction<T>(&mut self, future: T) -> Transaction<T>
    where
        T: Future,
        T::Error: From<Error>,
    {
        Transaction(proto::TransactionFuture::new(self.0.clone(), future))
    }

    pub fn batch_execute(&mut self, query: &str) -> BatchExecute {
        BatchExecute(self.0.batch_execute(query))
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Connection(proto::Connection);

impl Connection {
    pub fn cancel_data(&self) -> CancelData {
        self.0.cancel_data()
    }

    pub fn parameter(&self, name: &str) -> Option<&str> {
        self.0.parameter(name)
    }

    pub fn poll_message(&mut self) -> Poll<Option<AsyncMessage>, Error> {
        self.0.poll_message()
    }
}

impl Future for Connection {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Error> {
        self.0.poll()
    }
}

pub enum AsyncMessage {
    Notice(DbError),
    Notification(Notification),
    #[doc(hidden)]
    __NonExhaustive,
}

#[must_use = "futures do nothing unless polled"]
pub struct CancelQuery(proto::CancelFuture);

impl Future for CancelQuery {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Error> {
        self.0.poll()
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Handshake(proto::HandshakeFuture);

impl Future for Handshake {
    type Item = (Client, Connection);
    type Error = Error;

    fn poll(&mut self) -> Poll<(Client, Connection), Error> {
        let (client, connection) = try_ready!(self.0.poll());

        Ok(Async::Ready((Client(client), Connection(connection))))
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Prepare(proto::PrepareFuture);

impl Future for Prepare {
    type Item = Statement;
    type Error = Error;

    fn poll(&mut self) -> Poll<Statement, Error> {
        let statement = try_ready!(self.0.poll());

        Ok(Async::Ready(Statement(statement)))
    }
}

pub struct Statement(proto::Statement);

impl Statement {
    pub fn params(&self) -> &[Type] {
        self.0.params()
    }

    pub fn columns(&self) -> &[Column] {
        self.0.columns()
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Execute(proto::ExecuteFuture);

impl Future for Execute {
    type Item = u64;
    type Error = Error;

    fn poll(&mut self) -> Poll<u64, Error> {
        self.0.poll()
    }
}

#[must_use = "streams do nothing unless polled"]
pub struct Query(proto::QueryStream);

impl Stream for Query {
    type Item = Row;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Row>, Error> {
        match self.0.poll() {
            Ok(Async::Ready(Some(row))) => Ok(Async::Ready(Some(Row(row)))),
            Ok(Async::Ready(None)) => Ok(Async::Ready(None)),
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(e) => Err(e),
        }
    }
}

#[must_use = "streams do nothing unless polled"]
pub struct CopyOut(proto::CopyOutStream);

impl Stream for CopyOut {
    type Item = Bytes;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Bytes>, Error> {
        self.0.poll()
    }
}

pub struct Row(proto::Row);

impl Row {
    pub fn columns(&self) -> &[Column] {
        self.0.columns()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn get<'a, I, T>(&'a self, idx: I) -> T
    where
        I: RowIndex + fmt::Debug,
        T: FromSql<'a>,
    {
        self.0.get(idx)
    }

    pub fn try_get<'a, I, T>(&'a self, idx: I) -> Result<Option<T>, Error>
    where
        I: RowIndex,
        T: FromSql<'a>,
    {
        self.0.try_get(idx)
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct Transaction<T>(proto::TransactionFuture<T, T::Item, T::Error>)
where
    T: Future,
    T::Error: From<Error>;

impl<T> Future for Transaction<T>
where
    T: Future,
    T::Error: From<Error>,
{
    type Item = T::Item;
    type Error = T::Error;

    fn poll(&mut self) -> Poll<T::Item, T::Error> {
        self.0.poll()
    }
}

#[must_use = "futures do nothing unless polled"]
pub struct BatchExecute(proto::SimpleQueryFuture);

impl Future for BatchExecute {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Error> {
        self.0.poll()
    }
}
