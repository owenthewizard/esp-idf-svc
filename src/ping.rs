//! Send ICMP echo requests (Ping)
use core::{ffi, mem, ptr, time::Duration};

use ::log::*;

use crate::ipv4;
use crate::private::common::*;
use crate::private::waitable::*;
use crate::sys::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Configuration {
    pub count: u32,
    pub interval: Duration,
    pub timeout: Duration,
    pub data_size: u32,
    pub tos: u8,
}

impl Default for Configuration {
    fn default() -> Self {
        Self {
            count: 5,
            interval: Duration::from_secs(1),
            timeout: Duration::from_secs(1),
            data_size: 56,
            tos: 0,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Info {
    pub addr: ipv4::Ipv4Addr,
    pub seqno: u32,
    pub ttl: u8,
    pub elapsed_time: Duration,
    pub recv_len: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    Timeout,
    Success(Info),
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Summary {
    pub transmitted: u32,
    pub received: u32,
    pub time: Duration,
}

#[derive(Debug, Default)]
pub struct EspPing(u32);

unsafe impl Send for EspPing {}
unsafe impl Sync for EspPing {}

impl EspPing {
    pub const fn new(interface_index: u32) -> Self {
        Self(interface_index)
    }

    pub fn ping(&mut self, ip: ipv4::Ipv4Addr, conf: &Configuration) -> Result<Summary, EspError> {
        info!(
            "About to run a summary ping {} with configuration {:?}",
            ip, conf
        );

        let mut tracker = Tracker::new(Some(nop_callback));

        self.run_ping(ip, conf, &mut tracker)?;

        Ok(tracker.summary)
    }

    pub fn ping_details<F: FnMut(&Summary, &Reply) + Send>(
        &mut self,
        ip: ipv4::Ipv4Addr,
        conf: &Configuration,
        reply_callback: F,
    ) -> Result<Summary, EspError> {
        info!(
            "About to run a detailed ping {} with configuration {:?}",
            ip, conf
        );

        let mut tracker = Tracker::new(Some(reply_callback));

        self.run_ping(ip, conf, &mut tracker)?;

        Ok(tracker.summary)
    }

    fn run_ping<F: FnMut(&Summary, &Reply) + Send>(
        &self,
        ip: ipv4::Ipv4Addr,
        conf: &Configuration,
        tracker: &mut Tracker<F>,
    ) -> Result<(), EspError> {
        #[allow(clippy::needless_update)]
        #[allow(clippy::useless_conversion)]
        let config = esp_ping_config_t {
            count: conf.count,
            interval_ms: conf.interval.as_millis() as u32,
            timeout_ms: conf.timeout.as_millis() as u32,
            data_size: conf.data_size,
            tos: conf.tos.into(),
            target_addr: ip_addr_t {
                u_addr: ip_addr__bindgen_ty_1 {
                    ip4: Newtype::<ip4_addr_t>::from(ip).0,
                },
                type_: 0,
            },
            task_stack_size: 4096,
            task_prio: 2,
            interface: self.0,
            ttl: 64,
            ..Default::default()
        };

        let callbacks = esp_ping_callbacks_t {
            on_ping_success: Some(Self::on_ping_success::<F>),
            on_ping_timeout: Some(Self::on_ping_timeout::<F>),
            on_ping_end: Some(Self::on_ping_end::<F>),
            cb_args: ptr::addr_of_mut!(*tracker).cast(),
        };

        let mut handle: esp_ping_handle_t = ptr::null_mut();
        let handle_ref = &mut handle;

        esp!(unsafe {
            esp_ping_new_session(&config, &callbacks, ptr::addr_of_mut!(*handle_ref).cast())
        })?;

        if handle.is_null() {
            return Err(EspError::from_infallible::<ESP_ERR_INVALID_ARG>());
        }

        info!("Ping session established, got handle {:?}", handle);

        {
            let mut running = tracker.waitable.state.lock();
            *running = true;
        }

        esp!(unsafe { esp_ping_start(handle) })?;
        info!("Ping session started");

        info!("Waiting for the ping session to complete");

        tracker.waitable.wait_while(|running| Ok(*running))?;

        esp!(unsafe { esp_ping_stop(handle) })?;
        info!("Ping session stopped");

        esp!(unsafe { esp_ping_delete_session(handle) })?;

        info!("Ping session {:?} removed", &handle);

        Ok(())
    }

    unsafe extern "C" fn on_ping_success<F: FnMut(&Summary, &Reply) + Send>(
        handle: esp_ping_handle_t,
        args: *mut ffi::c_void,
    ) {
        info!("Ping success callback invoked");

        let tracker_ptr = args.cast::<Tracker<F>>();
        let tracker = tracker_ptr.as_mut().unwrap();

        let mut seqno: ffi::c_ushort = 0;
        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_SEQNO,
            ptr::addr_of_mut!(seqno).cast(),
            mem::size_of_val(&seqno) as u32,
        );

        let mut ttl: ffi::c_uchar = 0;
        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_TTL,
            ptr::addr_of_mut!(ttl).cast(),
            mem::size_of_val(&ttl) as u32,
        );

        let mut target_addr_raw = [0_u8; mem::size_of::<ip_addr_t>()];
        let target_addr: &mut ip_addr_t = mem::transmute(&mut target_addr_raw);

        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_IPADDR,
            ptr::addr_of!(*target_addr) as *mut _,
            mem::size_of::<ip_addr_t>() as _,
        );

        let mut elapsed_time: ffi::c_uint = 0;
        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_TIMEGAP,
            ptr::addr_of_mut!(elapsed_time).cast(),
            mem::size_of_val(&elapsed_time) as u32,
        );

        let mut recv_len: ffi::c_uint = 0;
        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_SIZE,
            ptr::addr_of_mut!(recv_len).cast(),
            mem::size_of_val(&recv_len) as u32,
        );

        let addr = ipv4::Ipv4Addr::from(Newtype(target_addr.u_addr.ip4));

        info!(
            "From {} icmp_seq={} ttl={} time={}ms bytes={}",
            addr, seqno, ttl, elapsed_time, recv_len
        );

        if let Some(reply_callback) = tracker.reply_callback.as_mut() {
            Self::update_summary(handle, &mut tracker.summary);

            reply_callback(
                &tracker.summary,
                &Reply::Success(Info {
                    addr,
                    seqno: u32::from(seqno),
                    ttl,
                    recv_len,
                    elapsed_time: Duration::from_millis(u64::from(elapsed_time)),
                }),
            );
        }
    }

    unsafe extern "C" fn on_ping_timeout<F: FnMut(&Summary, &Reply) + Send>(
        handle: esp_ping_handle_t,
        args: *mut ffi::c_void,
    ) {
        info!("Ping timeout callback invoked");

        let tracker_ptr = args.cast::<Tracker<F>>();
        let tracker = tracker_ptr.as_mut().unwrap();

        let mut seqno: ffi::c_ushort = 0;
        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_SEQNO,
            ptr::addr_of_mut!(seqno).cast(),
            mem::size_of_val(&seqno) as u32,
        );

        let mut target_addr_raw = [0_u8; mem::size_of::<ip_addr_t>()];
        let target_addr: &mut ip_addr_t = mem::transmute(&mut target_addr_raw);

        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_IPADDR,
            ptr::addr_of!(*target_addr) as *mut _,
            mem::size_of::<ip_addr_t>() as _,
        );

        info!("From {} icmp_seq={} timeout", "???", seqno);

        if let Some(reply_callback) = tracker.reply_callback.as_mut() {
            Self::update_summary(handle, &mut tracker.summary);

            reply_callback(&tracker.summary, &Reply::Timeout);
        }
    }

    #[allow(clippy::mutex_atomic)]
    unsafe extern "C" fn on_ping_end<F: FnMut(&Summary, &Reply) + Send>(
        handle: esp_ping_handle_t,
        args: *mut ffi::c_void,
    ) {
        info!("Ping end callback invoked");

        let tracker_ptr = args.cast::<Tracker<F>>();
        let tracker = tracker_ptr.as_mut().unwrap();

        Self::update_summary(handle, &mut tracker.summary);

        info!(
            "{} packets transmitted, {} received, time {}ms",
            tracker.summary.transmitted,
            tracker.summary.received,
            tracker.summary.time.as_millis()
        );

        let mut running = tracker.waitable.state.lock();
        *running = false;

        tracker.waitable.cvar.notify_all();
    }

    unsafe fn update_summary(handle: esp_ping_handle_t, summary: &mut Summary) {
        let mut transmitted: ffi::c_uint = 0;
        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_REQUEST,
            ptr::addr_of_mut!(transmitted).cast(),
            mem::size_of_val(&transmitted) as u32,
        );

        let mut received: ffi::c_uint = 0;
        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_REPLY,
            ptr::addr_of_mut!(received).cast(),
            mem::size_of_val(&received) as u32,
        );

        let mut total_time: ffi::c_uint = 0;
        esp_ping_get_profile(
            handle,
            esp_ping_profile_t_ESP_PING_PROF_DURATION,
            ptr::addr_of_mut!(total_time).cast(),
            mem::size_of_val(&total_time) as u32,
        );

        summary.transmitted = transmitted;
        summary.received = received;
        summary.time = Duration::from_millis(u64::from(total_time));
    }
}

struct Tracker<F: FnMut(&Summary, &Reply) + Send> {
    summary: Summary,
    waitable: Waitable<bool>,
    reply_callback: Option<F>,
}

impl<F: FnMut(&Summary, &Reply) + Send> Tracker<F> {
    #[allow(clippy::mutex_atomic)]
    pub fn new(reply_callback: Option<F>) -> Self {
        Self {
            summary: Summary::default(),
            waitable: Waitable::new(false),
            reply_callback,
        }
    }
}

const fn nop_callback(_summary: &Summary, _reply: &Reply) {}
