//! Synchronous rdma-core CM endpoints. The default verbs path never loads librdmacm.
use super::*;
use crate::resource::{Cleanup, Owner};
use crate::verbs::{Opcode, WorkRequest};
use crate::{PostStatus, operation::reserve_ids};
use std::{ffi::CString, net::SocketAddr};

type Id = *mut ffi::rdma_cm_id;
type Addr = *mut ffi::rdma_addrinfo;
struct CmApi {
    resolve: unsafe extern "C" fn(
        *const c_char,
        *const c_char,
        *const ffi::rdma_addrinfo,
        *mut Addr,
    ) -> c_int,
    free: unsafe extern "C" fn(Addr),
    create:
        unsafe extern "C" fn(*mut Id, Addr, *mut ffi::ibv_pd, *mut ffi::ibv_qp_init_attr) -> c_int,
    destroy: unsafe extern "C" fn(Id),
    listen: unsafe extern "C" fn(Id, c_int) -> c_int,
    request: unsafe extern "C" fn(Id, *mut Id) -> c_int,
    connect: unsafe extern "C" fn(Id, *mut ffi::rdma_conn_param) -> c_int,
    accept: unsafe extern "C" fn(Id, *mut ffi::rdma_conn_param) -> c_int,
    port: unsafe extern "C" fn(Id) -> u16,
    _lib: Library,
}
impl CmApi {
    fn load() -> RdmaResult<Self> {
        // SAFETY: the named system library provides the standard CM ABI from rdma_cma.h.
        let lib = unsafe { Library::new("librdmacm.so.1") }
            .map_err(|e| Error::Load(LoadError::Unavailable(e.to_string())))?;
        macro_rules! symbol { ($name:literal) => {{
            // SAFETY: the destination field specifies the matching header signature. The table owns the DSO.
            *unsafe { lib.get(concat!($name,"\0").as_bytes()) }.map_err(|e|Error::Load(LoadError::Unavailable(e.to_string())))?
        }}; }
        Ok(Self {
            resolve: symbol!("rdma_getaddrinfo"),
            free: symbol!("rdma_freeaddrinfo"),
            create: symbol!("rdma_create_ep"),
            destroy: symbol!("rdma_destroy_ep"),
            listen: symbol!("rdma_listen"),
            request: symbol!("rdma_get_request"),
            connect: symbol!("rdma_connect"),
            accept: symbol!("rdma_accept"),
            port: symbol!("rdma_get_src_port"),
            _lib: lib,
        })
    }
}
fn cm_check(op: &'static str, rc: i32) -> RdmaResult<()> {
    check(
        op,
        if rc == -1 {
            io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO)
        } else {
            rc
        },
    )
}
struct Endpoint {
    api: std::sync::Arc<CmApi>,
    id: NonNull<ffi::rdma_cm_id>,
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        // SAFETY: this is the sole endpoint owner; a connection holds it back while DMA is uncertain.
        unsafe { (self.api.destroy)(self.id.as_ptr()) };
    }
}
impl Endpoint {
    fn create(addr: SocketAddr, passive: bool) -> RdmaResult<Self> {
        let api = std::sync::Arc::new(CmApi::load()?);
        let node = CString::new(addr.ip().to_string()).map_err(|_| Error::Invalid("CM address"))?;
        let service =
            CString::new(addr.port().to_string()).map_err(|_| Error::Invalid("CM service"))?;
        let hints = ffi::rdma_addrinfo {
            ai_flags: (ffi::RAI_NUMERICHOST | if passive { ffi::RAI_PASSIVE } else { 0 }) as i32,
            ai_port_space: ffi::RDMA_PS_TCP as i32,
            ai_qp_type: ffi::IBV_QPT_RC as i32,
            ..Default::default()
        };
        let mut info = std::ptr::null_mut();
        // SAFETY: numeric C strings, hints and output pointer live through resolution.
        let rc = unsafe { (api.resolve)(node.as_ptr(), service.as_ptr(), &hints, &mut info) };
        cm_check("rdma_getaddrinfo", rc)?;
        let info = NonNull::new(info).ok_or(Error::Invalid("null CM address info"))?;
        let mut init = ffi::ibv_qp_init_attr {
            qp_type: ffi::IBV_QPT_RC,
            cap: ffi::ibv_qp_cap {
                max_send_wr: 1,
                max_recv_wr: 1,
                max_send_sge: 1,
                max_recv_sge: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut id = std::ptr::null_mut();
        // SAFETY: the resolver owns info; CM creates an endpoint with private QP and CQs.
        let rc = unsafe { (api.create)(&mut id, info.as_ptr(), std::ptr::null_mut(), &mut init) };
        // SAFETY: create_ep no longer references the resolver's address list.
        unsafe { (api.free)(info.as_ptr()) };
        cm_check("rdma_create_ep", rc)?;
        Ok(Self {
            api,
            id: NonNull::new(id).ok_or(Error::Invalid("null CM endpoint"))?,
        })
    }
}

/// A listening standard RDMA-CM RC endpoint. Accept waits for one incoming connection.
pub struct CmListener(Endpoint);
impl CmListener {
    pub fn bind(addr: SocketAddr, backlog: i32) -> RdmaResult<Self> {
        if backlog <= 0 {
            return Err(Error::Invalid("CM listen backlog"));
        }
        let ep = Endpoint::create(addr, true)?;
        // SAFETY: ep is a private passive CM endpoint.
        cm_check("rdma_listen", unsafe {
            (ep.api.listen)(ep.id.as_ptr(), backlog)
        })?;
        Ok(Self(ep))
    }
    pub fn local_port(&self) -> u16 {
        // SAFETY: the listener retains this bound CM id.
        u16::from_be(unsafe { (self.0.api.port)(self.0.id.as_ptr()) })
    }
    pub fn accept(&mut self) -> RdmaResult<CmConnection> {
        let mut id = std::ptr::null_mut();
        // SAFETY: the exclusively borrowed listener retains its event channel; id is writable.
        cm_check("rdma_get_request", unsafe {
            (self.0.api.request)(self.0.id.as_ptr(), &mut id)
        })?;
        let ep = Endpoint {
            api: self.0.api.clone(),
            id: NonNull::new(id).ok_or(Error::Invalid("null CM request"))?,
        };
        // SAFETY: CM owns the newly created child QP; null selects standard connection parameters.
        cm_check("rdma_accept", unsafe {
            (ep.api.accept)(ep.id.as_ptr(), std::ptr::null_mut())
        })?;
        CmConnection::new(ep)
    }
}

struct Posted {
    id: u64,
    receive: bool,
    bytes: Vec<u8>,
    mr: Option<Mr>,
}
struct Connection {
    _ep: Endpoint,
    verbs: &'static Native,
    qp: Qp,
    pd: Pd,
    send: Cq,
    recv: Cq,
    pending: Option<Posted>,
}
impl Connection {
    fn receipt(&self) -> RdmaResult<Option<std::result::Result<usize, u32>>> {
        let Some(p) = &self.pending else {
            return Ok(None);
        };
        let Some(wc) = self
            .verbs
            .poll_one(if p.receive { self.recv } else { self.send })?
        else {
            return Ok(None);
        };
        // SAFETY: the connection owns the QP through every outstanding message.
        let expected_qpn = unsafe { (*self.qp.0.as_ptr()).qp_num };
        let result = match wc {
            WorkCompletion::Success {
                id,
                qpn,
                opcode,
                bytes,
                ..
            } if id == p.id
                && qpn == expected_qpn
                && opcode
                    == if p.receive {
                        ffi::IBV_WC_RECV
                    } else {
                        ffi::IBV_WC_SEND
                    }
                && (!p.receive || bytes as usize <= p.bytes.len()) =>
            {
                Ok(if p.receive {
                    bytes as usize
                } else {
                    p.bytes.len()
                })
            }
            WorkCompletion::Error {
                id, qpn, status, ..
            } if id == p.id && qpn == expected_qpn => Err(status),
            _ => return Err(Error::Invalid("foreign or malformed CM completion")),
        };
        Ok(Some(result))
    }
}
impl Cleanup for Connection {
    fn cleanup(&mut self) -> std::result::Result<(), Fault> {
        if self.pending.is_none() {
            return Ok(());
        }
        self.verbs.error_state(self.qp)?;
        // Synchronous CM destroys its QP through a void API. Obtain the matching
        // success or flush CQE before deregistering. A missing receipt keeps every owner.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(2);
        loop {
            match self.receipt() {
                Ok(Some(_)) => break,
                Ok(None) if std::time::Instant::now() < deadline => std::thread::yield_now(),
                Err(Error::Native { operation, code }) => return Err(Fault(operation, code)),
                _ => return Err(Fault("CM message not quiesced", libc::EBUSY)),
            }
        }
        let p = self
            .pending
            .as_mut()
            .ok_or(Fault("CM pending message", libc::EINVAL))?;
        destroy(&mut p.mr, "ibv_dereg_mr", self.verbs.ibv_dereg_mr)?;
        self.pending.take();
        Ok(())
    }
}
/// A message recovered after a matching successful work completion and MR deregistration.
pub struct CmMessage {
    pub buffer: Vec<u8>,
    pub bytes: usize,
}
/// A synchronous CM connection with one outstanding SEND or RECV. Each post
/// takes an initialized Vec. Poll before reusing it or closing the connection.
pub struct CmConnection(Owner<Connection>);
impl CmConnection {
    pub fn connect(addr: SocketAddr) -> RdmaResult<Self> {
        let ep = Endpoint::create(addr, false)?;
        // SAFETY: ep owns an active RC id and CM-created QP.
        cm_check("rdma_connect", unsafe {
            (ep.api.connect)(ep.id.as_ptr(), std::ptr::null_mut())
        })?;
        Self::new(ep)
    }
    fn new(ep: Endpoint) -> RdmaResult<Self> {
        let verbs = get().map_err(Error::Load)?;
        // SAFETY: the established endpoint owns its QP, PD and completion queues.
        let id = unsafe { ep.id.as_ref() };
        let qp = handle(id.qp, "CM QP")?;
        let pd = handle(id.pd, "CM PD")?;
        let send = handle(id.send_cq, "CM send CQ")?;
        let recv = handle(id.recv_cq, "CM receive CQ")?;
        Ok(Self(Owner::new(Connection {
            _ep: ep,
            verbs,
            qp,
            pd,
            send,
            recv,
            pending: None,
        })))
    }
    pub fn send(&mut self, bytes: Vec<u8>) -> RdmaResult<PostStatus> {
        self.post(bytes, false)
    }
    pub fn receive(&mut self, buffer: Vec<u8>) -> RdmaResult<PostStatus> {
        self.post(buffer, true)
    }
    fn post(&mut self, mut bytes: Vec<u8>, receive: bool) -> RdmaResult<PostStatus> {
        let d = self.0.get_mut()?;
        if d.pending.is_some() {
            return Err(Error::Busy);
        }
        let length = u32::try_from(bytes.len())
            .ok()
            .filter(|n| *n > 0)
            .ok_or(Error::Invalid("CM message length"))?;
        let id = reserve_ids(1)?;
        let ptr = NonNull::new(bytes.as_mut_ptr()).ok_or(Error::Invalid("CM message address"))?;
        let mr = d.verbs.register(d.pd, ptr, bytes.len(), false)?;
        let mut wr = WorkRequest::new(
            id,
            if receive {
                Opcode::Receive
            } else {
                Opcode::Send
            },
            ptr.as_ptr() as usize as u64,
            length,
            local_key(mr),
        );
        d.pending = Some(Posted {
            id,
            receive,
            bytes,
            mr: Some(mr),
        });
        let posted = d.verbs.post_payload(d.qp, std::slice::from_mut(&mut wr));
        match posted {
            Ok(r) if r.accepted == Some(1) && r.code == 0 => Ok(PostStatus::Complete),
            Ok(r) if r.accepted != Some(0) => Ok(PostStatus::Uncertain { code: r.code }),
            other => {
                let err = match other {
                    Err(e) => e,
                    Ok(r) => Error::Native {
                        operation: "CM post",
                        code: r.code,
                    },
                };
                self.retire()?;
                Err(err)
            }
        }
    }
    fn retire(&mut self) -> RdmaResult<Posted> {
        self.0
            .cleanup_step(|d| {
                let p = d
                    .pending
                    .as_mut()
                    .ok_or(Fault("CM pending message", libc::EINVAL))?;
                destroy(&mut p.mr, "ibv_dereg_mr", d.verbs.ibv_dereg_mr)
            })
            .map_err(Fault::unquiesced)?;
        self.0.get_mut()?.pending.take().ok_or(Error::Closed)
    }
    pub fn poll(&mut self) -> RdmaResult<Option<CmMessage>> {
        let Some(result) = self.0.get()?.receipt()? else {
            return Ok(None);
        };
        let message = self.retire()?;
        let n = result.map_err(|status| Error::Native {
            operation: "CM work completion",
            code: status as i32,
        })?;
        Ok(Some(CmMessage {
            buffer: message.bytes,
            bytes: n,
        }))
    }
    /// Move the QP to ERR and collect a matching success or flush receipt before
    /// deregistration. Unproven drain keeps the native endpoint and backing together.
    pub fn abort(&mut self) -> RdmaResult<()> {
        self.0.release().map(drop)
    }
    /// Returns Busy while a message can still complete. Its owner may keep polling.
    pub fn close(&mut self) -> RdmaResult<()> {
        if self.0.get()?.pending.is_some() {
            return Err(Error::Busy);
        }
        self.0.release().map(drop)
    }
}
