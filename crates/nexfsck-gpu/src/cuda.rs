use super::BlockInterval;
use libc::{c_char, c_int, c_uint, c_void};
use std::ffi::{CStr, CString};
use std::mem;
use std::ptr;

type CuDevice = c_int;
type CuContext = *mut c_void;
type CuModule = *mut c_void;
type CuFunction = *mut c_void;
type CuDevicePtr = u64;
type CuResult = c_int;

type CuInit = unsafe extern "C" fn(c_uint) -> CuResult;
type CuDeviceGet = unsafe extern "C" fn(*mut CuDevice, c_int) -> CuResult;
type CuCtxCreate = unsafe extern "C" fn(*mut CuContext, c_uint, CuDevice) -> CuResult;
type CuCtxDestroy = unsafe extern "C" fn(CuContext) -> CuResult;
type CuModuleLoadData = unsafe extern "C" fn(*mut CuModule, *const c_void) -> CuResult;
type CuModuleUnload = unsafe extern "C" fn(CuModule) -> CuResult;
type CuModuleGetFunction =
    unsafe extern "C" fn(*mut CuFunction, CuModule, *const c_char) -> CuResult;
type CuMemAlloc = unsafe extern "C" fn(*mut CuDevicePtr, usize) -> CuResult;
type CuMemFree = unsafe extern "C" fn(CuDevicePtr) -> CuResult;
type CuMemcpyHtoD = unsafe extern "C" fn(CuDevicePtr, *const c_void, usize) -> CuResult;
type CuMemcpyDtoH = unsafe extern "C" fn(*mut c_void, CuDevicePtr, usize) -> CuResult;
type CuLaunchKernel = unsafe extern "C" fn(
    CuFunction,
    c_uint,
    c_uint,
    c_uint,
    c_uint,
    c_uint,
    c_uint,
    c_uint,
    *mut c_void,
    *mut *mut c_void,
    *mut *mut c_void,
) -> CuResult;
type CuCtxSynchronize = unsafe extern "C" fn() -> CuResult;

const PTX: &str = r#"
.version 6.0
.target sm_50
.address_size 64
.visible .entry collision_candidates(
    .param .u64 starts_ptr,
    .param .u64 counts_ptr,
    .param .u64 flags_ptr,
    .param .u32 item_count)
{
    .reg .pred %p<3>;
    .reg .b32 %r<7>;
    .reg .b64 %rd<12>;
    ld.param.u64 %rd1, [starts_ptr];
    ld.param.u64 %rd2, [counts_ptr];
    ld.param.u64 %rd3, [flags_ptr];
    ld.param.u32 %r1, [item_count];
    mov.u32 %r2, %ctaid.x;
    mov.u32 %r3, %ntid.x;
    mov.u32 %r4, %tid.x;
    mad.lo.s32 %r5, %r2, %r3, %r4;
    add.s32 %r6, %r5, 1;
    setp.ge.u32 %p1, %r6, %r1;
    @%p1 bra DONE;
    mul.wide.u32 %rd4, %r5, 8;
    add.s64 %rd5, %rd1, %rd4;
    ld.global.u64 %rd6, [%rd5];
    add.s64 %rd7, %rd5, 8;
    ld.global.u64 %rd8, [%rd7];
    mul.wide.u32 %rd9, %r5, 4;
    add.s64 %rd10, %rd2, %rd9;
    ld.global.u32 %r2, [%rd10];
    cvt.u64.u32 %rd11, %r2;
    add.s64 %rd6, %rd6, %rd11;
    setp.gt.u64 %p2, %rd6, %rd8;
    selp.u32 %r3, 1, 0, %p2;
    cvt.u64.u32 %rd4, %r5;
    add.s64 %rd3, %rd3, %rd4;
    st.global.u8 [%rd3], %r3;
DONE:
    ret;
}
"#;

pub struct CudaContext {
    library: *mut c_void,
    context: CuContext,
    module: CuModule,
    function: CuFunction,
    mem_alloc: CuMemAlloc,
    mem_free: CuMemFree,
    copy_to_device: CuMemcpyHtoD,
    copy_to_host: CuMemcpyDtoH,
    launch: CuLaunchKernel,
    synchronize: CuCtxSynchronize,
    module_unload: CuModuleUnload,
    context_destroy: CuCtxDestroy,
}

impl CudaContext {
    pub fn new() -> Result<Self, String> {
        unsafe {
            let library_name = CString::new("libcuda.so.1").unwrap();
            let library = libc::dlopen(library_name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
            if library.is_null() {
                return Err(dl_error());
            }
            let init: CuInit = symbol(library, b"cuInit\0")?;
            let device_get: CuDeviceGet = symbol(library, b"cuDeviceGet\0")?;
            let context_create: CuCtxCreate = symbol(library, b"cuCtxCreate_v2\0")?;
            let context_destroy = symbol(library, b"cuCtxDestroy_v2\0")?;
            let module_load: CuModuleLoadData = symbol(library, b"cuModuleLoadData\0")?;
            let module_unload = symbol(library, b"cuModuleUnload\0")?;
            let module_get_function: CuModuleGetFunction =
                symbol(library, b"cuModuleGetFunction\0")?;
            let mem_alloc = symbol(library, b"cuMemAlloc_v2\0")?;
            let mem_free = symbol(library, b"cuMemFree_v2\0")?;
            let copy_to_device = symbol(library, b"cuMemcpyHtoD_v2\0")?;
            let copy_to_host = symbol(library, b"cuMemcpyDtoH_v2\0")?;
            let launch = symbol(library, b"cuLaunchKernel\0")?;
            let synchronize = symbol(library, b"cuCtxSynchronize\0")?;
            check(init(0), "cuInit")?;
            let mut device = 0;
            check(device_get(&mut device, 0), "cuDeviceGet")?;
            let mut context = ptr::null_mut();
            check(context_create(&mut context, 0, device), "cuCtxCreate")?;
            let mut module = ptr::null_mut();
            let ptx = CString::new(PTX).unwrap();
            check(
                module_load(&mut module, ptx.as_ptr().cast()),
                "cuModuleLoadData",
            )?;
            let mut function = ptr::null_mut();
            check(
                module_get_function(
                    &mut function,
                    module,
                    b"collision_candidates\0".as_ptr().cast(),
                ),
                "cuModuleGetFunction",
            )?;
            Ok(Self {
                library,
                context,
                module,
                function,
                mem_alloc,
                mem_free,
                copy_to_device,
                copy_to_host,
                launch,
                synchronize,
                module_unload,
                context_destroy,
            })
        }
    }

    pub fn find_collision_candidates(
        &self,
        intervals: &[BlockInterval],
    ) -> Result<Vec<usize>, String> {
        if intervals.len() < 2 {
            return Ok(Vec::new());
        }
        let starts: Vec<u64> = intervals.iter().map(|v| v.start_block).collect();
        let counts: Vec<u32> = intervals.iter().map(|v| v.block_count).collect();
        let mut flags = vec![0u8; intervals.len()];
        unsafe {
            let starts_d = self.allocate_copy(&starts)?;
            let counts_d = self.allocate_copy(&counts)?;
            let mut flags_d = 0;
            check(
                (self.mem_alloc)(&mut flags_d, flags.len()),
                "cuMemAlloc(flags)",
            )?;
            let mut starts_arg = starts_d;
            let mut counts_arg = counts_d;
            let mut flags_arg = flags_d;
            let mut count_arg = intervals.len() as u32;
            let mut params = [
                (&mut starts_arg as *mut CuDevicePtr).cast(),
                (&mut counts_arg as *mut CuDevicePtr).cast(),
                (&mut flags_arg as *mut CuDevicePtr).cast(),
                (&mut count_arg as *mut u32).cast(),
            ];
            let blocks = (intervals.len() as u32 + 255) / 256;
            let result = check(
                (self.launch)(
                    self.function,
                    blocks,
                    1,
                    1,
                    256,
                    1,
                    1,
                    0,
                    ptr::null_mut(),
                    params.as_mut_ptr(),
                    ptr::null_mut(),
                ),
                "cuLaunchKernel",
            )
            .and_then(|_| check((self.synchronize)(), "cuCtxSynchronize"))
            .and_then(|_| {
                check(
                    (self.copy_to_host)(flags.as_mut_ptr().cast(), flags_d, flags.len()),
                    "cuMemcpyDtoH",
                )
            });
            (self.mem_free)(starts_d);
            (self.mem_free)(counts_d);
            (self.mem_free)(flags_d);
            result?;
        }
        Ok(flags
            .into_iter()
            .enumerate()
            .filter_map(|(i, flag)| (flag != 0).then_some(i))
            .collect())
    }

    unsafe fn allocate_copy<T>(&self, values: &[T]) -> Result<CuDevicePtr, String> {
        let bytes = mem::size_of_val(values);
        let mut device = 0;
        check((self.mem_alloc)(&mut device, bytes), "cuMemAlloc")?;
        if let Err(error) = check(
            (self.copy_to_device)(device, values.as_ptr().cast(), bytes),
            "cuMemcpyHtoD",
        ) {
            (self.mem_free)(device);
            return Err(error);
        }
        Ok(device)
    }
}

impl Drop for CudaContext {
    fn drop(&mut self) {
        unsafe {
            (self.module_unload)(self.module);
            (self.context_destroy)(self.context);
            libc::dlclose(self.library);
        }
    }
}

unsafe fn symbol<T: Copy>(library: *mut c_void, name: &[u8]) -> Result<T, String> {
    let pointer = libc::dlsym(library, name.as_ptr().cast());
    if pointer.is_null() {
        Err(dl_error())
    } else {
        Ok(mem::transmute_copy(&pointer))
    }
}
fn check(result: CuResult, operation: &str) -> Result<(), String> {
    if result == 0 {
        Ok(())
    } else {
        Err(format!("{operation} failed with CUDA error {result}"))
    }
}
fn dl_error() -> String {
    unsafe {
        let error = libc::dlerror();
        if error.is_null() {
            "dynamic loader error".into()
        } else {
            CStr::from_ptr(error).to_string_lossy().into_owned()
        }
    }
}
