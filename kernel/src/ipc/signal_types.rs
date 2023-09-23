use core::{ffi::c_void, mem::size_of, sync::atomic::AtomicI64};

use alloc::{sync::Arc, vec::Vec};

use crate::{
    arch::{
        asm::bitops::ffz,
        ipc::signal::{SigCode, SigFlags, SigSet, Signal, _NSIG},
    },
    include::bindings::bindings::siginfo,
    libs::ffi_convert::{FFIBind2Rust, __convert_mut, __convert_ref},
    process::Pid,
    syscall::{user_access::UserBufferWriter, SystemError},
};

/// 存储信号处理函数的地址(来自用户态)
pub type __signalfn_t = u64;
pub type __sighandler_t = __signalfn_t;
/// 存储信号处理恢复函数的地址(来自用户态)
pub type __sigrestorer_fn_t = u64;
pub type __sigrestorer_t = __sigrestorer_fn_t;

pub const MAX_SIG_NUM: usize = _NSIG;

/// 用户态程序传入的SIG_DFL的值
pub const USER_SIG_DFL: u64 = 0;
/// 用户态程序传入的SIG_IGN的值
pub const USER_SIG_IGN: u64 = 1;
/// 用户态程序传入的SIG_ERR的值
pub const USER_SIG_ERR: u64 = 2;

// 因为 Rust 编译器不能在常量声明中正确识别级联的 "|" 运算符(experimental feature： https://github.com/rust-lang/rust/issues/67792)，因此
// 暂时只能通过这种方法来声明这些常量
pub const SIG_KERNEL_ONLY_MASK: SigSet =
    Signal::into_sigset(Signal::SIGSTOP).union(Signal::into_sigset(Signal::SIGKILL));

pub const SIG_KERNEL_STOP_MASK: SigSet = Signal::into_sigset(Signal::SIGSTOP)
    .union(Signal::into_sigset(Signal::SIGTSTP))
    .union(Signal::into_sigset(Signal::SIGTTIN))
    .union(Signal::into_sigset(Signal::SIGTTOU));

pub const SIG_KERNEL_COREDUMP_MASK: SigSet = Signal::into_sigset(Signal::SIGQUIT)
    .union(Signal::into_sigset(Signal::SIGILL))
    .union(Signal::into_sigset(Signal::SIGTRAP))
    .union(Signal::into_sigset(Signal::SIGABRT_OR_IOT))
    .union(Signal::into_sigset(Signal::SIGFPE))
    .union(Signal::into_sigset(Signal::SIGSEGV))
    .union(Signal::into_sigset(Signal::SIGBUS))
    .union(Signal::into_sigset(Signal::SIGSYS))
    .union(Signal::into_sigset(Signal::SIGXCPU))
    .union(Signal::into_sigset(Signal::SIGXFSZ));

pub const SIG_KERNEL_IGNORE_MASK: SigSet = Signal::into_sigset(Signal::SIGCONT)
    .union(Signal::into_sigset(Signal::SIGFPE))
    .union(Signal::into_sigset(Signal::SIGSEGV))
    .union(Signal::into_sigset(Signal::SIGBUS))
    .union(Signal::into_sigset(Signal::SIGTRAP))
    .union(Signal::into_sigset(Signal::SIGCHLD))
    .union(Signal::into_sigset(Signal::SIGIO_OR_POLL))
    .union(Signal::into_sigset(Signal::SIGSYS));

/// SignalStruct 在 pcb 中加锁
#[derive(Debug)]
pub struct SignalStruct {
    pub cnt: AtomicI64,
    pub handler: Arc<SigHandStruct>,
}

impl Default for SignalStruct {
    fn default() -> Self {
        Self {
            cnt: Default::default(),
            handler: Default::default(),
        }
    }
}

#[derive(Debug, Copy, Clone)]
pub enum SigactionType {
    SaHandler(SaHandlerType),
    SaSigaction(
        Option<
            unsafe extern "C" fn(
                sig: ::core::ffi::c_int,
                sinfo: *mut siginfo,
                arg1: *mut ::core::ffi::c_void,
            ),
        >,
    ),
}

impl SigactionType {
    /// Returns `true` if the sigaction type is [`SaHandler`].
    ///
    /// [`SaHandler`]: SigactionType::SaHandler
    #[must_use]
    pub fn is_sa_handler(&self) -> bool {
        matches!(self, Self::SaHandler(..))
    }
}

#[derive(Debug, Copy, Clone)]
pub enum SaHandlerType {
    SigError,
    SigDefault,
    SigIgnore,
    SigCustomized(__sighandler_t),
}

impl Into<usize> for SaHandlerType {
    fn into(self) -> usize {
        match self {
            Self::SigError => 2 as usize,
            Self::SigIgnore => 1 as usize,
            Self::SigDefault => 0 as usize,
            Self::SigCustomized(handler) => handler as usize,
        }
    }
}

impl SaHandlerType {
    /// Returns `true` if the sa handler type is [`SigDefault`].
    ///
    /// [`SigDefault`]: SaHandlerType::SigDefault
    #[must_use]
    pub fn is_sig_default(&self) -> bool {
        matches!(self, Self::SigDefault)
    }

    /// Returns `true` if the sa handler type is [`SigIgnore`].
    ///
    /// [`SigIgnore`]: SaHandlerType::SigIgnore
    #[must_use]
    pub fn is_sig_ignore(&self) -> bool {
        matches!(self, Self::SigIgnore)
    }

    /// Returns `true` if the sa handler type is [`SigError`].
    ///
    /// [`SigError`]: SaHandlerType::SigError
    #[must_use]
    pub fn is_sig_error(&self) -> bool {
        matches!(self, Self::SigError)
    }
}

/// 信号处理结构体
///
#[derive(Debug, Copy, Clone)]
pub struct Sigaction {
    action: SigactionType,
    flags: SigFlags,
    mask: SigSet, // 为了可扩展性而设置的sa_mask
    /// 信号处理函数执行结束后，将会跳转到这个函数内进行执行，然后执行sigreturn系统调用
    restorer: Option<__sigrestorer_t>,
}

impl Default for Sigaction {
    fn default() -> Self {
        Self {
            action: SigactionType::SaHandler(SaHandlerType::SigDefault),
            flags: Default::default(),
            mask: Default::default(),
            restorer: Default::default(),
        }
    }
}

impl Sigaction {
    pub fn ignore(&self, _sig: Signal) -> bool {
        if self.flags.contains(SigFlags::SA_FLAG_IGN) {
            return true;
        }
        //sa_flags为SA_FLAG_DFL,但是默认处理函数为忽略的情况的判断
        if self.flags().contains(SigFlags::SA_FLAG_DFL) {
            if let SigactionType::SaHandler(SaHandlerType::SigIgnore) = self.action {
                return true;
            }
        }
        return false;
    }
    pub fn new(
        action: SigactionType,
        flags: SigFlags,
        mask: SigSet,
        restorer: Option<__sigrestorer_t>,
    ) -> Self {
        Self {
            action,
            flags,
            mask,
            restorer,
        }
    }

    pub fn action(&self) -> SigactionType {
        self.action
    }

    pub fn flags(&self) -> SigFlags {
        self.flags
    }

    pub fn restorer(&self) -> Option<u64> {
        self.restorer
    }

    pub fn flags_mut(&mut self) -> &mut SigFlags {
        &mut self.flags
    }

    pub fn set_action(&mut self, action: SigactionType) {
        self.action = action;
    }

    pub fn mask(&self) -> SigSet {
        self.mask
    }

    pub fn mask_mut(&mut self) -> &mut SigSet {
        &mut self.mask
    }

    pub fn set_restorer(&mut self, restorer: Option<__sigrestorer_t>) {
        self.restorer = restorer;
    }
}

/// 用户态传入的sigaction结构体（符合posix规范）
/// 请注意，我们会在sys_sigaction函数里面将其转换成内核使用的sigaction结构体
#[derive(Debug)]
pub struct UserSigaction {
    pub handler: *mut core::ffi::c_void,
    pub sigaction: *mut core::ffi::c_void,
    pub mask: SigSet,
    pub flags: SigFlags,
    pub restorer: *mut core::ffi::c_void,
}

/**
 * siginfo中，根据signal的来源不同，该info中对应了不同的数据./=
 * 请注意，该info最大占用16字节
 */

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct SigInfo {
    sig_no: i32,
    sig_code: SigCode,
    errno: i32,
    reserved: u32,
    sig_type: SigType,
}

impl SigInfo {
    pub fn sig_no(&self) -> i32 {
        self.sig_no
    }

    pub fn sig_code(&self) -> SigCode {
        self.sig_code
    }

    pub fn errno(&self) -> i32 {
        self.errno
    }

    pub fn reserved(&self) -> u32 {
        self.reserved
    }

    pub fn sig_type(&self) -> SigType {
        self.sig_type
    }

    pub fn set_sig_type(&mut self, sig_type: SigType) {
        self.sig_type = sig_type;
    }
    /// @brief 将siginfo结构体拷贝到用户栈
    /// ## 参数
    ///
    /// `to` 用户空间指针
    ///
    /// ## 注意
    ///
    /// 该函数对应Linux中的https://opengrok.ringotek.cn/xref/linux-6.1.9/kernel/signal.c#3323
    /// Linux还提供了 https://opengrok.ringotek.cn/xref/linux-6.1.9/kernel/signal.c#3383 用来实现
    /// kernel_siginfo 保存到 用户的 compact_siginfo 的功能，但是我们系统内还暂时没有对这两种
    /// siginfo做区分，因此暂时不需要第二个函数
    pub fn copy_siginfo_to_user(&self, to: *mut SigInfo) -> Result<i32, SystemError> {
        // 验证目标地址是否为用户空间
        let mut user_buffer = UserBufferWriter::new(to, size_of::<SigInfo>(), true)?;

        let retval: Result<i32, SystemError> = Ok(0);

        user_buffer.copy_one_to_user(self, 0)?;
        return retval;
    }
}

#[derive(Copy, Clone, Debug)]
pub enum SigType {
    Kill(Pid),
}

impl SigInfo {
    pub fn new(
        sig: Signal,
        sig_errno: i32,
        sig_code: SigCode,
        reserved: u32,
        sig_type: SigType,
    ) -> Self {
        Self {
            sig_no: sig as i32,
            sig_code,
            errno: sig_errno,
            reserved,
            sig_type,
        }
    }
}

/// 在获取SigHandStruct的外部就获取到了锁，所以这里是不会有任何竞争的，只是处于内部可变性的需求
/// 才使用了SpinLock，这里并不会带来太多的性能开销
#[derive(Debug)]
pub struct SigHandStruct(pub [Sigaction; MAX_SIG_NUM as usize]);

impl Default for SigHandStruct {
    fn default() -> Self {
        SigHandStruct([Sigaction::default(); MAX_SIG_NUM as usize])
    }
}

#[derive(Debug)]
pub struct SigPending {
    signal: SigSet,
    queue: SigQueue,
}

impl Default for SigPending {
    fn default() -> Self {
        SigPending {
            signal: SigSet::default(),
            queue: SigQueue::default(),
        }
    }
}

impl SigPending {
    pub fn signal(&self) -> SigSet {
        self.signal
    }

    pub fn queue(&self) -> &SigQueue {
        &self.queue
    }

    pub fn queue_mut(&mut self) -> &mut SigQueue {
        &mut self.queue
    }

    pub fn signal_mut(&mut self) -> &mut SigSet {
        &mut self.signal
    }
    /// @brief 获取下一个要处理的信号（sig number越小的信号，优先级越高）
    ///
    /// @param pending 等待处理的信号
    /// @param sig_mask 屏蔽了的信号
    /// @return i32 下一个要处理的信号的number. 如果为0,则无效
    pub fn next_signal(&self, sig_mask: &SigSet) -> Signal {
        let mut sig = Signal::INVALID;

        let s = self.signal();
        let m = *sig_mask;

        // 获取第一个待处理的信号的号码
        let x = s.intersection(m.complement());
        if x.bits() != 0 {
            sig = Signal::from(ffz(x.complement().bits()) + 1);
            return sig;
        }

        // 暂时只支持64种信号
        assert_eq!(_NSIG, 64);

        return sig;
    }
    /// @brief 收集信号的信息
    ///
    /// @param sig 要收集的信号的信息
    /// @param pending 信号的排队等待标志
    /// @return SigInfo 信号的信息
    pub fn collect_signal(&mut self, sig: Signal) -> SigInfo {
        let (info, still_pending) = self.queue_mut().find_and_delete(sig);

        // 如果没有仍在等待的信号，则清除pending位
        if !still_pending {
            self.signal_mut().remove(sig.into());
        }

        if info.is_some() {
            return info.unwrap();
        } else {
            // 信号不在sigqueue中，这意味着当前信号是来自快速路径，因此直接把siginfo设置为0即可。
            let mut ret = SigInfo::new(sig, 0, SigCode::SI_USER, 0, SigType::Kill(Pid::from(0)));
            ret.set_sig_type(SigType::Kill(Pid::new(0)));
            return ret;
        }
    }
}

/// @brief 进程接收到的信号的队列
#[derive(Debug, Clone)]
pub struct SigQueue {
    pub q: Vec<SigInfo>,
}

#[allow(dead_code)]
impl SigQueue {
    /// @brief 初始化一个新的信号队列
    pub fn new(capacity: usize) -> Self {
        SigQueue {
            q: Vec::with_capacity(capacity),
        }
    }

    /// @brief 在信号队列中寻找第一个满足要求的siginfo, 并返回它的引用
    ///
    /// @return (第一个满足要求的siginfo的引用; 是否有多个满足条件的siginfo)
    pub fn find(&self, sig: Signal) -> (Option<&SigInfo>, bool) {
        // 是否存在多个满足条件的siginfo
        let mut still_pending = false;
        let mut info: Option<&SigInfo> = None;

        for x in self.q.iter() {
            if x.sig_no == sig as i32 {
                if info.is_some() {
                    still_pending = true;
                    break;
                } else {
                    info = Some(x);
                }
            }
        }
        return (info, still_pending);
    }

    /// @brief 在信号队列中寻找第一个满足要求的siginfo, 并将其从队列中删除，然后返回这个siginfo
    ///
    /// @return (第一个满足要求的siginfo; 从队列中删除前是否有多个满足条件的siginfo)
    pub fn find_and_delete(&mut self, sig: Signal) -> (Option<SigInfo>, bool) {
        // 是否存在多个满足条件的siginfo
        let mut still_pending = false;
        let mut first = true; // 标记变量，记录当前是否已经筛选出了一个元素

        let filter = |x: &mut SigInfo| {
            if x.sig_no == sig as i32 {
                if !first {
                    // 如果之前已经筛选出了一个元素，则不把当前元素删除
                    still_pending = true;
                    return false;
                } else {
                    // 当前是第一个被筛选出来的元素
                    first = false;
                    return true;
                }
            }
            return false;
        };
        // 从sigqueue中过滤出结果
        let mut filter_result: Vec<SigInfo> = self.q.drain_filter(filter).collect();
        // 筛选出的结果不能大于1个
        assert!(filter_result.len() <= 1);

        return (filter_result.pop(), still_pending);
    }

    /// @brief 从sigqueue中删除mask中被置位的信号。也就是说，比如mask的第1位被置为1,那么就从sigqueue中删除所有signum为2的信号的信息。
    pub fn flush_by_mask(&mut self, mask: &SigSet) {
        // 定义过滤器，从sigqueue中删除mask中被置位的信号
        let filter = |x: &mut SigInfo| {
            if mask.contains(SigSet::from_bits_truncate(x.sig_no as u64)) {
                return true;
            }

            return false;
        };
        let filter_result: Vec<SigInfo> = self.q.drain_filter(filter).collect();
        // 回收这些siginfo
        for x in filter_result {
            drop(x)
        }
    }

    /// @brief 从C的void*指针转换为static生命周期的可变引用
    pub fn from_c_void(p: *mut c_void) -> &'static mut SigQueue {
        let sq = p as *mut SigQueue;
        let sq = unsafe { sq.as_mut::<'static>() }.unwrap();
        return sq;
    }
}

impl Default for SigQueue {
    fn default() -> Self {
        Self {
            q: Default::default(),
        }
    }
}

/// @brief 将给定的signal_struct解析为Rust的signal.rs中定义的signal_struct的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::signal_struct> for SignalStruct {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::signal_struct,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::signal_struct,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的siginfo解析为Rust的signal.rs中定义的siginfo的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::siginfo> for SigInfo {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::siginfo,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::siginfo,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的sigset_t解析为Rust的signal.rs中定义的sigset_t的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::sigset_t> for SigSet {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::sigset_t,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::sigset_t,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的sigpending解析为Rust的signal.rs中定义的sigpending的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的
impl FFIBind2Rust<crate::include::bindings::bindings::sigpending> for SigPending {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::sigpending,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::sigpending,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的来自bindgen的sighand_struct解析为Rust的signal.rs中定义的sighand_struct的引用
///
/// 这么做的主要原因在于，由于PCB是通过bindgen生成的FFI，因此pcb中的结构体类型都是bindgen自动生成的，会导致无法自定义功能的问题。
impl FFIBind2Rust<crate::include::bindings::bindings::sighand_struct> for SigHandStruct {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::sighand_struct,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::sighand_struct,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}

/// @brief 将给定的来自bindgen的sigaction解析为Rust的signal.rs中定义的sigaction的引用
impl FFIBind2Rust<crate::include::bindings::bindings::sigaction> for Sigaction {
    fn convert_mut(
        src: *mut crate::include::bindings::bindings::sigaction,
    ) -> Option<&'static mut Self> {
        return __convert_mut(src);
    }
    fn convert_ref(
        src: *const crate::include::bindings::bindings::sigaction,
    ) -> Option<&'static Self> {
        return __convert_ref(src);
    }
}
