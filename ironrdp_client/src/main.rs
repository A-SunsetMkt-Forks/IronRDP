mod config;
mod connection_sequence;
mod utils;

use std::{
    io::{self, Write},
    net::TcpStream,
    sync::Arc,
};

use failure::Fail;
use ironrdp::nego;
use log::{debug, error};

use self::{config::Config, connection_sequence::*};

pub type RdpResult<T> = Result<T, RdpError>;

mod danger {
    pub struct NoCertificateVerification {}

    impl rustls::ServerCertVerifier for NoCertificateVerification {
        fn verify_server_cert(
            &self,
            _roots: &rustls::RootCertStore,
            _presented_certs: &[rustls::Certificate],
            _dns_name: webpki::DNSNameRef<'_>,
            _ocsp: &[u8],
        ) -> Result<rustls::ServerCertVerified, rustls::TLSError> {
            Ok(rustls::ServerCertVerified::assertion())
        }
    }
}

fn main() {
    let config = Config::parse_args();
    setup_logging(config.log_file.clone()).expect("failed to initialize logging");

    match run(config) {
        Ok(_) => {
            println!("RDP connection sequence finished");
            std::process::exit(exitcode::OK);
        }
        Err(e) => {
            error!("{}", e);
            println!("RDP failed because of {}", e);

            let code = match e {
                RdpError::IOError(_) => exitcode::IOERR,
                RdpError::ConnectionError(_) => exitcode::NOHOST,
                _ => exitcode::PROTOCOL,
            };
            std::process::exit(code);
        }
    }
}

fn setup_logging(log_file: String) -> Result<(), fern::InitError> {
    fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "{}[{}] {}",
                chrono::Local::now().format("[%Y-%m-%d][%H:%M:%S:%6f]"),
                record.level(),
                message
            ))
        })
        .chain(fern::log_file(log_file)?)
        .apply()?;

    Ok(())
}

fn run(config: Config) -> RdpResult<()> {
    let addr = utils::socket_addr_to_string(config.routing_addr);
    let stream = TcpStream::connect(addr.as_str()).map_err(RdpError::ConnectionError)?;
    let mut stream = utils::StreamWrapper::new(stream);

    let (mut transport, selected_protocol) = DataTransport::connect(
        &mut stream,
        config.input.security_protocol,
        config.input.credentials.username.clone(),
    )?;
    let mut stream = stream.into_inner();

    let mut client_config = rustls::ClientConfig::default();
    client_config
        .dangerous()
        .set_certificate_verifier(Arc::new(danger::NoCertificateVerification {}));
    let config_ref = Arc::new(client_config);
    let dns_name = webpki::DNSNameRef::try_from_ascii_str("stub_string").unwrap();
    let mut tls_session = rustls::ClientSession::new(&config_ref, dns_name);

    let mut tls_stream = rustls::Stream::new(&mut tls_session, &mut stream);
    // handshake
    tls_stream.flush()?;

    let mut tls_stream = utils::StreamWrapper::new(tls_stream);

    if selected_protocol.contains(nego::SecurityProtocol::HYBRID)
        || selected_protocol.contains(nego::SecurityProtocol::HYBRID_EX)
    {
        process_cred_ssp(&mut tls_stream, config.input.credentials.clone())?;

        if selected_protocol.contains(nego::SecurityProtocol::HYBRID_EX) {
            if let sspi::internal::EarlyUserAuthResult::AccessDenied =
                EarlyUserAuthResult::read(tls_stream.get_reader())?
            {
                return Err(RdpError::AccessDenied);
            }
        }
    }

    let static_channels =
        process_mcs_connect(&mut tls_stream, &mut transport, &config, selected_protocol)?;

    let mut transport = McsTransport::new(transport);
    let joined_static_channels = process_mcs(&mut tls_stream, &mut transport, static_channels)?;
    debug!("Joined static channels: {:?}", joined_static_channels);

    let global_channel_id = *joined_static_channels
        .get(&*GLOBAL_CHANNEL_NAME)
        .expect("global channel must be added");
    let initiator_id = *joined_static_channels
        .get(&*USER_CHANNEL_NAME)
        .expect("user channel must be added");

    let mut transport = SendDataContextTransport::new(transport, initiator_id, global_channel_id);
    send_client_info(&mut tls_stream, &mut transport, &config)?;
    process_server_license(&mut tls_stream.get_reader(), &mut transport)?;

    let mut transport = ShareControlHeaderTransport::new(transport, initiator_id);
    process_capability_sets(&mut tls_stream, &mut transport, &config)?;

    let mut transport = ShareDataHeaderTransport::new(transport);
    process_finalization(&mut tls_stream, &mut transport, initiator_id)?;

    Ok(())
}

#[derive(Debug, Fail)]
pub enum RdpError {
    #[fail(display = "IO error: {}", _0)]
    IOError(#[fail(cause)] io::Error),
    #[fail(display = "connection error: {}", _0)]
    ConnectionError(#[fail(cause)] io::Error),
    #[fail(display = "X.224 error: {}", _0)]
    X224Error(#[fail(cause)] io::Error),
    #[fail(display = "negotiation error: {}", _0)]
    NegotiationError(#[fail(cause)] ironrdp::nego::NegotiationError),
    #[fail(display = "unexpected PDU: {}", _0)]
    UnexpectedPdu(String),
    #[fail(display = "invalid response: {}", _0)]
    InvalidResponse(String),
    #[fail(display = "TLS connector error: {}", _0)]
    TlsConnectorError(rustls::TLSError),
    #[fail(display = "TLS handshake error: {}", _0)]
    TlsHandshakeError(rustls::TLSError),
    #[fail(display = "CredSSP error: {}", _0)]
    CredSspError(#[fail(cause)] sspi::Error),
    #[fail(display = "CredSSP TSRequest error: {}", _0)]
    TsRequestError(#[fail(cause)] io::Error),
    #[fail(display = "early User Authentication Result error: {}", _0)]
    EarlyUserAuthResultError(#[fail(cause)] io::Error),
    #[fail(display = "the server denied access via Early User Authentication Result")]
    AccessDenied,
    #[fail(display = "MCS Connect error: {}", _0)]
    McsConnectError(#[fail(cause)] ironrdp::McsError),
    #[fail(display = "failed to get info about the user: {}", _0)]
    UserInfoError(String),
    #[fail(display = "MCS error: {}", _0)]
    McsError(ironrdp::McsError),
    #[fail(display = "Client Info PDU error: {}", _0)]
    ClientInfoError(ironrdp::rdp::RdpError),
    #[fail(display = "Server License PDU error: {}", _0)]
    ServerLicenseError(ironrdp::rdp::RdpError),
    #[fail(display = "Share Control Header error: {}", _0)]
    ShareControlHeaderError(ironrdp::rdp::RdpError),
    #[fail(display = "capability sets error: {}", _0)]
    CapabilitySetsError(ironrdp::rdp::RdpError),
}

impl From<io::Error> for RdpError {
    fn from(e: io::Error) -> Self {
        RdpError::IOError(e)
    }
}

impl From<rustls::TLSError> for RdpError {
    fn from(e: rustls::TLSError) -> Self {
        match e {
            rustls::TLSError::InappropriateHandshakeMessage { .. }
            | rustls::TLSError::HandshakeNotComplete => RdpError::TlsHandshakeError(e),
            _ => RdpError::TlsConnectorError(e),
        }
    }
}

impl From<ironrdp::nego::NegotiationError> for RdpError {
    fn from(e: ironrdp::nego::NegotiationError) -> Self {
        RdpError::NegotiationError(e)
    }
}

impl From<ironrdp::McsError> for RdpError {
    fn from(e: ironrdp::McsError) -> Self {
        RdpError::McsError(e)
    }
}
