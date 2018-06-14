use std::net::SocketAddr;

use futures::future::{self, Future, Either};

use ::future_ext::ResultWithContextExt;
use ::error::{
    ConnectingFailed,
    LogicError
};
use ::data_types::Domain;
use ::common::{
    TlsConfig, SetupTls,
    ClientIdentity, DefaultTlsSetup
};
use ::io::{Io, SmtpResult};
use ::connection::{
    Connection, Cmd,
};

/// A future resolving to an `Connection` instance
pub type ConnectingFuture = Box<Future<Item=Connection, Error=ConnectingFailed> + Send + 'static>;

fn cmd_future2connecting_future<LE: 'static, E>(
    res: Result<(Connection, SmtpResult), E>,
    new_logic_err: LE
) -> impl Future<Item=Connection, Error=ConnectingFailed> + Send
    where LE: Send + FnOnce(LogicError) -> ConnectingFailed,
          E: Into<ConnectingFailed>
{
    let fut =
        match res {
            Err(err) => Either::A(future::err(err.into())),
            Ok((con, Ok(_resp))) => Either::A(future::ok(con.into())),
            Ok((con, Err(err))) => {
                Either::B(con.quit().then(|_| Err(new_logic_err(err))))
            }
        };

    fut
}

impl Connection {

    /// open a connection to an smtp server using given configuration
    pub fn connect<S, A>(config: ConnectionConfig<A, S>)
        -> impl Future<Item=Connection, Error=ConnectingFailed> + Send
        where S: SetupTls, A: Cmd + Send
    {
        let ConnectionConfig { addr, security, client_id, auth_cmd } = config;

        #[allow(deprecated)]
        let con_fut = match security {
            Security::None => {
                Either::B(Either::A(Connection::_connect_insecure(&addr, client_id)))
            },
            Security::DirectTls(tls_config) => {
                Either::B(Either::B(Connection::_connect_direct_tls(&addr, client_id, tls_config)))
            }
            Security::StartTls(tls_config) => {
                Either::A(Connection::_connect_starttls(&addr, client_id, tls_config))
            }
        };

        let fut = con_fut
            .and_then(|con| con
                .send(auth_cmd)
                .then(|res| cmd_future2connecting_future(res, ConnectingFailed::Auth))
            );

        fut
    }

    #[doc(hidden)]
    pub fn _connect_insecure_no_ehlo(addr: &SocketAddr)
        -> impl Future<Item=Connection, Error=ConnectingFailed> + Send
    {
        let fut = Io
            ::connect_insecure(addr)
            .and_then(Io::parse_response)
            .then(|res| {
                let res = res.map(|(io, res)| (Connection::from(io), res));
                cmd_future2connecting_future(res, ConnectingFailed::Setup)
            });

        fut
    }

    #[doc(hidden)]
    pub fn _connect_direct_tls_no_ehlo<S>(addr: &SocketAddr, config: TlsConfig<S>)
        -> impl Future<Item=Connection, Error=ConnectingFailed> + Send
        where S: SetupTls
    {
        let fut = Io
            ::connect_secure(addr, config)
            .and_then(Io::parse_response)
            .then(|res| {
                let res = res.map(|(io, res)| (Connection::from(io), res));
                cmd_future2connecting_future(res, ConnectingFailed::Setup)
            });

        fut
    }

    #[doc(hidden)]
    pub fn _connect_insecure(addr: &SocketAddr, clid: ClientIdentity)
        -> impl Future<Item=Connection, Error=ConnectingFailed> + Send
    {
        //Note: this has a circular dependency between Connection <-> cmd Ehlo which
        // could be resolved using a ext. trait, but it's more ergonomic this way
        use command::Ehlo;
        let fut = Connection
            ::_connect_insecure_no_ehlo(addr)
            .and_then(|con| con
                .send(Ehlo::from(clid))
                .then(|res| cmd_future2connecting_future(res, ConnectingFailed::Setup))
            );


        fut
    }

    #[doc(hidden)]
    pub fn _connect_direct_tls<S>(
        addr: &SocketAddr,
        clid: ClientIdentity,
        config: TlsConfig<S>,
    ) -> impl Future<Item=Connection, Error=ConnectingFailed> + Send
        where S: SetupTls
    {
        //Note: this has a circular dependency between Connection <-> cmd Ehlo which
        // could be resolved using a ext. trait, but it's more ergonomic this way
        use command::Ehlo;
        let fut = Connection
            ::_connect_direct_tls_no_ehlo(addr, config)
            .and_then(|con| con
                .send(Ehlo::from(clid))
                .then(|res| cmd_future2connecting_future(res, ConnectingFailed::Setup))
            );

        fut
    }

    #[doc(hidden)]
    pub fn _connect_starttls<S>(
        addr: &SocketAddr,
        clid: ClientIdentity,
        config: TlsConfig<S>
    )
        -> impl Future<Item=Connection, Error=ConnectingFailed> + Send
        where S: SetupTls
    {
        //Note: this has a circular dependency between Connection <-> cmd StartTls/Ehlo which
        // could be resolved using a ext. trait, but it's more ergonomic this way
        use command::{StartTls, Ehlo};
        let TlsConfig { domain, setup } = config;

        let fut = Connection
            ::_connect_insecure(&addr, clid.clone())
            .and_then(|con| con
                .send(StartTls {
                    setup_tls: setup,
                    sni_domain: domain
                })
                .map_err(ConnectingFailed::Io)
            )
            .ctx_and_then(|con, _| con
                .send(Ehlo::from(clid))
                .map_err(ConnectingFailed::Io)
            )
            .then(|res| cmd_future2connecting_future(res, ConnectingFailed::Setup));

        fut
    }
}

/// configure what kind of security is used
#[derive(Debug, Clone)]
pub enum Security<S>
    where S: SetupTls
{
    /// use a plain non encrypted connection
    #[deprecated(
        since="0.0",
        note="it's strongly discourage to use unencrypted connections for private information/auth etc.")]
    None,
    /// directly connect with TCP-TLS to smtp server
    DirectTls(TlsConfig<S>),
    /// connect with just TCP and then start TLS with the STARTTLS command
    StartTls(TlsConfig<S>)
}

#[derive(Debug, Clone)]
pub struct ConnectionConfig<A, S = DefaultTlsSetup>
    where S: SetupTls, A: Cmd
{
    /// the address and port to connect to (i.e. the ones of the smtp server)
    pub addr: SocketAddr,
    /// a command used for authentication (use NOOP if you don't auth)
    pub auth_cmd: A,
    /// the kind of TLS mechanism used when setting up the connection
    pub security: Security<S>,
    /// the client identity, i.e. your "identity"
    ///
    /// This is relevant for the communication between smtp server, through
    /// for connecting to an MSA (e.g. thunderbird connecting to gmail)
    /// using localhost (`[127.0.0.1]`) is enough
    pub client_id: ClientIdentity
}

//IMPROVE: potentially crate a type safe builder chain
// e.g. ConnectionBuilder
//      ::connect_with_tls(addr, domain)/::connect_with_starttls(addr, domain)
//      .identity(clientidentity) / .identitfy_as_localhost()
//      .auth(cmd) / .build() //uses auth Nop
//      .build()
impl<A> ConnectionConfig<A, DefaultTlsSetup>
    where A: Cmd
{

    /// create a connection config using direct tls
    ///
    /// This uses the default tls setup. The passed
    /// in domain is the domain in the certificate
    /// of the server used to make sure you connected
    /// to the right server (e.g. `smtp.ethereal.email`)
    pub fn with_direct_tls(addr: SocketAddr, domain: Domain, clid: ClientIdentity, auth_cmd: A) -> Self {
        ConnectionConfig {
            addr, auth_cmd,
            security: Security::DirectTls(domain.into()),
            client_id: clid
        }
    }

    /// create a connection config using starttls
    ///
    /// This uses the default tls setup. The passed
    /// in domain is the domain in the certificate
    /// of the server used to make sure you connected
    /// to the right server (e.g. `smtp.ethereal.email`)
    pub fn with_starttls(addr: SocketAddr, domain: Domain, clid: ClientIdentity, auth_cmd: A) -> Self {
        ConnectionConfig {
            addr, auth_cmd,
            security: Security::StartTls(domain.into()),
            client_id: clid
        }
    }
}