extern crate schannel;

use self::schannel::cert_context::{CertContext, HashAlgorithm, KeySpec};
use self::schannel::cert_store::{CertAdd, CertStore, Memory, PfxImportOptions};
use self::schannel::crypt_prov::{AcquireOptions, ProviderType};
use self::schannel::schannel_cred::{Direction, Protocol, SchannelCred};
use self::schannel::tls_stream;
use std::error;
use std::fmt;
use std::io;
use std::str;
use std::ffi::OsStr;
use std::path::{PathBuf};

use {TlsAcceptorBuilder, TlsConnectorBuilder};

const SEC_E_NO_CREDENTIALS: u32 = 0x8009030E;

static PROTOCOLS: &'static [Protocol] = &[
    Protocol::Ssl3,
    Protocol::Tls10,
    Protocol::Tls11,
    Protocol::Tls12,
];

fn convert_protocols(min: Option<::Protocol>, max: Option<::Protocol>) -> &'static [Protocol] {
    let mut protocols = PROTOCOLS;
    if let Some(p) = max.and_then(|max| protocols.get(..=max as usize)) {
        protocols = p;
    }
    if let Some(p) = min.and_then(|min| protocols.get(min as usize..)) {
        protocols = p;
    }
    protocols
}

pub struct Error(io::Error);

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        error::Error::source(&self.0)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Display::fmt(&self.0, fmt)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Error {
        Error(error)
    }
}

#[derive(Clone)]
pub struct Identity {
    cert: CertContext,
}

// used for the from_os_provider function
enum OsProviderParameters {
    ContextFromStore {
        store_name: String,
        is_machine: bool,
        decoded_hex: Vec<u8>,
    },
    ContextFromFile {
        file_path: PathBuf,
    },
}

impl Identity {
    pub fn from_pkcs12(buf: &[u8], pass: &str) -> Result<Identity, Error> {
        let store = PfxImportOptions::new().password(pass).import(buf)?;
        let mut identity = None;

        for cert in store.certs() {
            if cert
                .private_key()
                .silent(true)
                .compare_key(true)
                .acquire()
                .is_ok()
            {
                identity = Some(cert);
                break;
            }
        }

        let identity = match identity {
            Some(identity) => identity,
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "No identity found in PKCS #12 archive",
                )
                .into());
            }
        };

        Ok(Identity { cert: identity })
    }

    pub fn from_pkcs8(pem: &[u8], key: &[u8]) -> Result<Identity, Error> {
        if !key.starts_with(b"-----BEGIN PRIVATE KEY-----") {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a PKCS#8 key").into());
        }

        let mut store = Memory::new()?.into_store();
        let mut cert_iter = pem::PemBlock::new(pem).into_iter();
        let leaf = cert_iter.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "at least one certificate must be provided to create an identity",
            )
        })?;
        let cert = CertContext::from_pem(std::str::from_utf8(leaf).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "leaf cert contains invalid utf8",
            )
        })?)?;

        let name = gen_container_name();
        let mut options = AcquireOptions::new();
        options.container(&name);
        let type_ = ProviderType::rsa_full();

        let mut container = match options.acquire(type_) {
            Ok(container) => container,
            Err(_) => options.new_keyset(true).acquire(type_)?,
        };
        container.import().import_pkcs8_pem(&key)?;

        cert.set_key_prov_info()
            .container(&name)
            .type_(type_)
            .keep_open(true)
            .key_spec(KeySpec::key_exchange())
            .set()?;
        let mut context = store.add_cert(&cert, CertAdd::Always)?;

        for int_cert in cert_iter {
            let certificate = Certificate::from_pem(int_cert)?;
            context = store.add_cert(&certificate.0, CertAdd::Always)?;
        }
        Ok(Identity { cert: context })
    }

    pub fn from_os_provider(_pem: &[u8], provider_name: &OsStr, os_engine_string: &OsStr) -> Result<Identity, Error> {
        if provider_name != "ncrypt" && provider_name != "e_ncrypt" {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,"`provider_name` must be either ncrypt or e_ncypt").into())
        } 

        let os_provider = parse_engine_string(os_engine_string)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput,"Invalid `os_engine_string`"))?;

        match os_provider {
                OsProviderParameters::ContextFromStore {store_name, is_machine, decoded_hex} => {

                let store = if is_machine {CertStore::open_local_machine(&store_name)} else {CertStore::open_current_user(&store_name)}?;
                let mut identity = None;
            
                for cert in store.certs() { 

                    let algo: HashAlgorithm; 
                    match decoded_hex.len() {
                        20 => algo = HashAlgorithm::sha1(), 
                        32 => algo = HashAlgorithm::sha256(), 
                        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput,"Invalid hex thumbprint").into())
                    }

                    let hash = cert.fingerprint(algo)?; 
                    if hash == decoded_hex { 
                        if cert 
                            .private_key() 
                            .silent(true) 
                            .compare_key(true) 
                            .acquire() 
                            .is_err() 
                        { 
                            return Err(io::Error::new(io::ErrorKind::InvalidInput,"Missing or invalid private key property").into())
                        } 
                        identity = Some(cert);
                        break; 
                    } 
                } 

                let identity = match identity {
                    Some(identity) => identity,
                    None => {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput,"No identity found in provided store").into());
                    }
                };
                return Ok(Identity { cert: identity })
            }

            OsProviderParameters::ContextFromFile {file_path} => {
                let store = CertStore::open_file(&file_path.as_path())?;
                // set identity to the cert that matches the key inside the pfx file
                let mut identity = None;
                for cert in store.certs() {
                    if cert
                        .private_key()
                        .silent(true)
                        .compare_key(true)
                        .acquire()
                        .is_ok()
                    {
                        identity = Some(cert);
                        break;
                    }
                }
                let identity = match identity {
                    Some(identity) => identity,
                    None => {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput,"No identity found in provided store").into());
                    }
                };
                return Ok(Identity { cert: identity })
            }
        }
    }
}


// The name of the container must be unique to have multiple active keys.
fn gen_container_name() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    format!("native-tls-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn parse_engine_string(engine_string: &OsStr) -> io::Result<OsProviderParameters> { 

    let converted_str = engine_string.to_string_lossy(); // convert back to to_string, hanlde error
    if let Some((first, second)) = converted_str.split_once(":") {

    if first == "file" {
        let path = PathBuf::from(second);
        let context_from_file = OsProviderParameters::ContextFromFile { file_path: path };  
        return Ok(context_from_file); 
    } 

    if first == "user" || first == "machine" {
        let parts: Vec<&str> = second.split(':').map(str::trim).collect();
        if parts.len() != 2 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput,"Invalid string passed").into())
        }            

        let decoded_hex = hex::decode(parts[1].to_string())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput,"Hex decode failed"))?;
        
        let context_from_store = OsProviderParameters::ContextFromStore {
            is_machine: if first == "machine" {true} else {false},
            store_name: parts[0].to_string(),
            decoded_hex: decoded_hex
        };
        return Ok(context_from_store);
        }
    }
    return Err(io::Error::new(io::ErrorKind::InvalidInput,"Invalid string passed").into())
}

#[derive(Clone)]
pub struct Certificate(CertContext);

impl Certificate {
    pub fn from_der(buf: &[u8]) -> Result<Certificate, Error> {
        let cert = CertContext::new(buf)?;
        Ok(Certificate(cert))
    }

    pub fn from_pem(buf: &[u8]) -> Result<Certificate, Error> {
        match str::from_utf8(buf) {
            Ok(s) => {
                let cert = CertContext::from_pem(s)?;
                Ok(Certificate(cert))
            }
            Err(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "PEM representation contains non-UTF-8 bytes",
            )
            .into()),
        }
    }

    pub fn to_der(&self) -> Result<Vec<u8>, Error> {
        Ok(self.0.to_der().to_vec())
    }
}

pub struct MidHandshakeTlsStream<S>(tls_stream::MidHandshakeTlsStream<S>);

impl<S> fmt::Debug for MidHandshakeTlsStream<S>
where
    S: fmt::Debug,
{
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl<S> MidHandshakeTlsStream<S> {
    pub fn get_ref(&self) -> &S {
        self.0.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.0.get_mut()
    }
}

impl<S> MidHandshakeTlsStream<S>
where
    S: io::Read + io::Write,
{
    pub fn handshake(self) -> Result<TlsStream<S>, HandshakeError<S>> {
        match self.0.handshake() {
            Ok(s) => Ok(TlsStream(s)),
            Err(e) => Err(e.into()),
        }
    }
}

pub enum HandshakeError<S> {
    Failure(Error),
    WouldBlock(MidHandshakeTlsStream<S>),
}

impl<S> From<tls_stream::HandshakeError<S>> for HandshakeError<S> {
    fn from(e: tls_stream::HandshakeError<S>) -> HandshakeError<S> {
        match e {
            tls_stream::HandshakeError::Failure(e) => HandshakeError::Failure(e.into()),
            tls_stream::HandshakeError::Interrupted(s) => {
                HandshakeError::WouldBlock(MidHandshakeTlsStream(s))
            }
        }
    }
}

impl<S> From<io::Error> for HandshakeError<S> {
    fn from(e: io::Error) -> HandshakeError<S> {
        HandshakeError::Failure(e.into())
    }
}

#[derive(Clone, Debug)]
pub struct TlsConnector {
    cert: Option<CertContext>,
    roots: CertStore,
    min_protocol: Option<::Protocol>,
    max_protocol: Option<::Protocol>,
    use_sni: bool,
    accept_invalid_hostnames: bool,
    accept_invalid_certs: bool,
    disable_built_in_roots: bool,
    #[cfg(feature = "alpn")]
    alpn: Vec<String>,
}

impl TlsConnector {
    pub fn new(builder: &TlsConnectorBuilder) -> Result<TlsConnector, Error> {
        let cert = builder.identity.as_ref().map(|i| i.0.cert.clone());
        let mut roots = Memory::new()?.into_store();
        for cert in &builder.root_certificates {
            roots.add_cert(&(cert.0).0, CertAdd::ReplaceExisting)?;
        }

        Ok(TlsConnector {
            cert,
            roots,
            min_protocol: builder.min_protocol,
            max_protocol: builder.max_protocol,
            use_sni: builder.use_sni,
            accept_invalid_hostnames: builder.accept_invalid_hostnames,
            accept_invalid_certs: builder.accept_invalid_certs,
            disable_built_in_roots: builder.disable_built_in_roots,
            #[cfg(feature = "alpn")]
            alpn: builder.alpn.clone(),
        })
    }

    pub fn connect<S>(&self, domain: &str, stream: S) -> Result<TlsStream<S>, HandshakeError<S>>
    where
        S: io::Read + io::Write,
    {
        let mut builder = SchannelCred::builder();
        builder.enabled_protocols(convert_protocols(self.min_protocol, self.max_protocol));
        if let Some(cert) = self.cert.as_ref() {
            builder.cert(cert.clone());
        }
        let cred = builder.acquire(Direction::Outbound)?;
        let mut builder = tls_stream::Builder::new();
        builder
            .cert_store(self.roots.clone())
            .domain(domain)
            .use_sni(self.use_sni)
            .accept_invalid_hostnames(self.accept_invalid_hostnames);
        if self.accept_invalid_certs {
            builder.verify_callback(|_| Ok(()));
        } else if self.disable_built_in_roots {
            let roots_copy = self.roots.clone();
            builder.verify_callback(move |res| {
                if let Err(err) = res.result() {
                    // Propagate previous error encountered during normal cert validation.
                    return Err(err);
                }

                if let Some(chain) = res.chain() {
                    if chain
                        .certificates()
                        .any(|cert| roots_copy.certs().any(|root_cert| root_cert == cert))
                    {
                        return Ok(());
                    }
                }

                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "unable to find any user-specified roots in the final cert chain",
                ))
            });
        }
        #[cfg(feature = "alpn")]
        {
            if !self.alpn.is_empty() {
                builder.request_application_protocols(
                    &self.alpn.iter().map(|s| s.as_bytes()).collect::<Vec<_>>(),
                );
            }
        }
        match builder.connect(cred, stream) {
            Ok(s) => Ok(TlsStream(s)),
            Err(e) => Err(e.into()),
        }
    }
}

#[derive(Clone)]
pub struct TlsAcceptor {
    cert: CertContext,
    min_protocol: Option<::Protocol>,
    max_protocol: Option<::Protocol>,
}

impl TlsAcceptor {
    pub fn new(builder: &TlsAcceptorBuilder) -> Result<TlsAcceptor, Error> {
        Ok(TlsAcceptor {
            cert: builder.identity.0.cert.clone(),
            min_protocol: builder.min_protocol,
            max_protocol: builder.max_protocol,
        })
    }

    pub fn accept<S>(&self, stream: S) -> Result<TlsStream<S>, HandshakeError<S>>
    where
        S: io::Read + io::Write,
    {
        let mut builder = SchannelCred::builder();
        builder.enabled_protocols(convert_protocols(self.min_protocol, self.max_protocol));
        builder.cert(self.cert.clone());
        // FIXME we're probably missing the certificate chain?
        let cred = builder.acquire(Direction::Inbound)?;
        match tls_stream::Builder::new().accept(cred, stream) {
            Ok(s) => Ok(TlsStream(s)),
            Err(e) => Err(e.into()),
        }
    }
}

pub struct TlsStream<S>(tls_stream::TlsStream<S>);

impl<S: fmt::Debug> fmt::Debug for TlsStream<S> {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&self.0, fmt)
    }
}

impl<S> TlsStream<S> {
    pub fn get_ref(&self) -> &S {
        self.0.get_ref()
    }

    pub fn get_mut(&mut self) -> &mut S {
        self.0.get_mut()
    }
}

impl<S: io::Read + io::Write> TlsStream<S> {
    pub fn buffered_read_size(&self) -> Result<usize, Error> {
        Ok(self.0.get_buf().len())
    }

    pub fn peer_certificate(&self) -> Result<Option<Certificate>, Error> {
        match self.0.peer_certificate() {
            Ok(cert) => Ok(Some(Certificate(cert))),
            Err(ref e) if e.raw_os_error() == Some(SEC_E_NO_CREDENTIALS as i32) => Ok(None),
            Err(e) => Err(Error(e)),
        }
    }

    #[cfg(feature = "alpn")]
    pub fn negotiated_alpn(&self) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.0.negotiated_application_protocol()?)
    }

    pub fn tls_server_end_point(&self) -> Result<Option<Vec<u8>>, Error> {
        let cert = if self.0.is_server() {
            self.0.certificate()
        } else {
            self.0.peer_certificate()
        };

        let cert = match cert {
            Ok(cert) => cert,
            Err(ref e) if e.raw_os_error() == Some(SEC_E_NO_CREDENTIALS as i32) => return Ok(None),
            Err(e) => return Err(Error(e)),
        };

        let signature_algorithms = cert.sign_hash_algorithms()?;
        let hash = match signature_algorithms.rsplit('/').next().unwrap() {
            "MD5" | "SHA1" | "SHA256" => HashAlgorithm::sha256(),
            "SHA384" => HashAlgorithm::sha384(),
            "SHA512" => HashAlgorithm::sha512(),
            _ => return Ok(None),
        };

        let digest = cert.fingerprint(hash)?;
        Ok(Some(digest))
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        self.0.shutdown()?;
        Ok(())
    }
}

impl<S: io::Read + io::Write> io::Read for TlsStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read(buf)
    }
}

impl<S: io::Read + io::Write> io::Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

mod pem {
    /// Split data by PEM guard lines
    pub struct PemBlock<'a> {
        pem_block: &'a str,
        cur_end: usize,
    }

    impl<'a> PemBlock<'a> {
        pub fn new(data: &'a [u8]) -> PemBlock<'a> {
            let s = ::std::str::from_utf8(data).unwrap();
            PemBlock {
                pem_block: s,
                cur_end: s.find("-----BEGIN").unwrap_or(s.len()),
            }
        }
    }

    impl<'a> Iterator for PemBlock<'a> {
        type Item = &'a [u8];
        fn next(&mut self) -> Option<Self::Item> {
            let last = self.pem_block.len();
            if self.cur_end >= last {
                return None;
            }
            let begin = self.cur_end;
            let pos = self.pem_block[begin + 1..].find("-----BEGIN");
            self.cur_end = match pos {
                Some(end) => end + begin + 1,
                None => last,
            };
            return Some(&self.pem_block[begin..self.cur_end].as_bytes());
        }
    }
}

#[cfg(test)]
mod tests{
    use std::fs;
    use super::*;
    use std::env; 

    #[test]
    fn test_split() {
        // Split three certs, CRLF line terminators.
        use imp::pem::PemBlock;
        assert_eq!(
            PemBlock::new(
                b"-----BEGIN FIRST-----\r\n-----END FIRST-----\r\n\
            -----BEGIN SECOND-----\r\n-----END SECOND\r\n\
            -----BEGIN THIRD-----\r\n-----END THIRD\r\n"
            )
            .collect::<Vec<&[u8]>>(),
            vec![
                b"-----BEGIN FIRST-----\r\n-----END FIRST-----\r\n" as &[u8],
                b"-----BEGIN SECOND-----\r\n-----END SECOND\r\n",
                b"-----BEGIN THIRD-----\r\n-----END THIRD\r\n"
            ]
        );
        // Split three certs, CRLF line terminators except at EOF.
        assert_eq!(
            PemBlock::new(
                b"-----BEGIN FIRST-----\r\n-----END FIRST-----\r\n\
            -----BEGIN SECOND-----\r\n-----END SECOND-----\r\n\
            -----BEGIN THIRD-----\r\n-----END THIRD-----"
            )
            .collect::<Vec<&[u8]>>(),
            vec![
                b"-----BEGIN FIRST-----\r\n-----END FIRST-----\r\n" as &[u8],
                b"-----BEGIN SECOND-----\r\n-----END SECOND-----\r\n",
                b"-----BEGIN THIRD-----\r\n-----END THIRD-----"
            ]
        );
        // Split two certs, LF line terminators.
        assert_eq!(
            PemBlock::new(
                b"-----BEGIN FIRST-----\n-----END FIRST-----\n\
            -----BEGIN SECOND-----\n-----END SECOND\n"
            )
            .collect::<Vec<&[u8]>>(),
            vec![
                b"-----BEGIN FIRST-----\n-----END FIRST-----\n" as &[u8],
                b"-----BEGIN SECOND-----\n-----END SECOND\n"
            ]
        );
        // Split two certs, CR line terminators.
        assert_eq!(
            PemBlock::new(
                b"-----BEGIN FIRST-----\r-----END FIRST-----\r\
            -----BEGIN SECOND-----\r-----END SECOND\r"
            )
            .collect::<Vec<&[u8]>>(),
            vec![
                b"-----BEGIN FIRST-----\r-----END FIRST-----\r" as &[u8],
                b"-----BEGIN SECOND-----\r-----END SECOND\r"
            ]
        );
        // Split two certs, LF line terminators except at EOF.
        assert_eq!(
            PemBlock::new(
                b"-----BEGIN FIRST-----\n-----END FIRST-----\n\
            -----BEGIN SECOND-----\n-----END SECOND"
            )
            .collect::<Vec<&[u8]>>(),
            vec![
                b"-----BEGIN FIRST-----\n-----END FIRST-----\n" as &[u8],
                b"-----BEGIN SECOND-----\n-----END SECOND"
            ]
        );
        // Split a single cert, LF line terminators.
        assert_eq!(
            PemBlock::new(b"-----BEGIN FIRST-----\n-----END FIRST-----\n").collect::<Vec<&[u8]>>(),
            vec![b"-----BEGIN FIRST-----\n-----END FIRST-----\n" as &[u8]]
        );
        // Split a single cert, LF line terminators except at EOF.
        assert_eq!(
            PemBlock::new(b"-----BEGIN FIRST-----\n-----END FIRST-----").collect::<Vec<&[u8]>>(),
            vec![b"-----BEGIN FIRST-----\n-----END FIRST-----" as &[u8]]
        );
        // (Don't) split garbage.
        assert_eq!(
            PemBlock::new(b"junk").collect::<Vec<&[u8]>>(),
            Vec::<&[u8]>::new()
        );
        assert_eq!(
            PemBlock::new(b"junk-----BEGIN garbage").collect::<Vec<&[u8]>>(),
            vec![b"-----BEGIN garbage" as &[u8]]
        );
    }

    #[test]
    fn test_parse_engine_string() {
        
        let my_os_str = OsStr::new("user:my:7b78a8e15d5ddfccaa71088ee44606981bb804d7");
        let my_file_str = OsStr::new(r"file:C:\Microsoft.Autopilot.Security\MachineFunctionCerts\CY2TEAP00013459.CY2Test01.sst"); // have to prefix with "r" for raw string
        let file_result = parse_engine_string(my_file_str).unwrap();
        let os_result = parse_engine_string(my_os_str).unwrap();

        // Verify file parse
        assert!(matches!(file_result, OsProviderParameters::ContextFromFile{..}));
        match file_result {
            OsProviderParameters::ContextFromFile {file_path} => {
                assert_eq!(file_path.as_os_str(), r"C:\Microsoft.Autopilot.Security\MachineFunctionCerts\CY2TEAP00013459.CY2Test01.sst");
            }
            _ => {}
        }

        // Verify store parse
        assert!(matches!(os_result, OsProviderParameters::ContextFromStore{..}));

        match os_result {
            OsProviderParameters::ContextFromStore {store_name, is_machine, decoded_hex} => {
                let expected_hex = hex::decode("7b78a8e15d5ddfccaa71088ee44606981bb804d7").unwrap();
                assert_eq!(false, is_machine);
                assert_eq!(store_name, "my".to_string());
                assert_eq!(expected_hex, decoded_hex)
            }
            _ => {}
        }
    }

    #[test]
    fn test_os_provider_os_string() {
        let my_os_str = OsStr::new("user:RustMy:3e2e13a694b3ed9e40849a4ab98b2c84d1b714d8");
        let pfx_file = include_bytes!("../test/playserver_openssl2.pfx");
        let memory_store = PfxImportOptions::new()
            .include_extended_properties(true)
            .password("openssl")
            .import(pfx_file)
            .unwrap();

        let mut identity = None;
        for cert in memory_store.certs() {
            if cert
                .private_key()
                .silent(true)
                .compare_key(true)
                .acquire()
                .is_ok()
            {
                identity = Some(cert);
                break;
            }
        }

        let identity = identity.unwrap();

        let mut store = CertStore::open_current_user("RustMy").unwrap();
        store.add_cert(&identity, CertAdd::Always).unwrap();

        let unused_pem:[u8; 0] = [];

        let os_identity = Identity::from_os_provider(&unused_pem, OsStr::new("ncrypt"), my_os_str).unwrap();

        assert_eq!(os_identity.cert.private_key().silent(true).compare_key(true).acquire().is_ok(), true);
        assert_eq!(identity.fingerprint(HashAlgorithm::sha256()).unwrap(), os_identity.cert.fingerprint(HashAlgorithm::sha256()).unwrap());

        let _result = CertStore::delete_cert_and_key(identity);
        CertStore::delete_current_user_store("RustMy");
    }


    #[test]
    fn test_os_provider_sst_file() {

        let pfx_file = include_bytes!("../test/playserver_openssl2.pfx");
        let mut memory_store = PfxImportOptions::new()
            .include_extended_properties(true)
            .password("openssl")
            .import(pfx_file)
            .unwrap();

        let dir = env::temp_dir().join("test_sst.sst");
        let os_str = format!(r"file:{}", dir.to_str().unwrap());     
        let my_os_str = OsStr::new(os_str.as_str());


        CertStore::create_sst(&dir, &mut memory_store).unwrap();

        let mut identity = None;
        for cert in memory_store.certs() {
            if cert
                .private_key()
                .silent(true)
                .compare_key(true)
                .acquire()
                .is_ok()
            {
                identity = Some(cert);
                break;
            }
        }
        let identity = identity.unwrap();

        let unused_pem:[u8; 0] = [];

        let os_identity = Identity::from_os_provider(&unused_pem, OsStr::new("ncrypt"), my_os_str).unwrap();

        assert_eq!(os_identity.cert.private_key().silent(true).compare_key(true).acquire().is_ok(), true);
        assert_eq!(identity.fingerprint(HashAlgorithm::sha256()).unwrap(), os_identity.cert.fingerprint(HashAlgorithm::sha256()).unwrap());

        let _result = CertStore::delete_cert_and_key(identity);
        fs::remove_file(dir).unwrap();
    }
}