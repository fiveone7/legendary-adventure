// SPDX-License-Identifier: GPL-2.0-or-later
/*
 * Copyright 2021 Google LLC.
 */

 use std::alloc::{alloc, dealloc, Layout};
 use std::ptr;
 use std::slice;
 use std::mem;
 use std::io::{self, Read, Write};
 use std::fs::File;
 use std::path::Path;
 use std::os::unix::io::AsRawFd;
 use std::ffi::CString;
 use std::sync::Arc;
 use std::sync::Mutex;
 
 use libc::{c_void, size_t, ssize_t, malloc, free, memcpy, memcmp};
 use libz_sys::{z_stream, inflateInit2_, inflate, inflateEnd};
 use xz2::stream::{Stream, Action};
 use zstd_sys::{ZSTD_createDCtx, ZSTD_freeDCtx, ZSTD_decompressDCtx, ZSTD_DCtx};
 
 #[derive(Debug)]
 struct LoadInfo {
     pages: Vec<*mut c_void>,
     max_pages: usize,
     used_pages: usize,
     hdr: *mut c_void,
     len: usize,
     compressed_len: usize,
 }
 
 impl LoadInfo {
     fn new() -> Self {
         LoadInfo {
             pages: Vec::new(),
             max_pages: 0,
             used_pages: 0,
             hdr: ptr::null_mut(),
             len: 0,
             compressed_len: 0,
         }
     }
 }
 
 fn module_extend_max_pages(info: &mut LoadInfo, extent: usize) -> Result<(), io::Error> {
     let new_pages_len = info.max_pages + extent;
     let new_pages = unsafe {
         let layout = Layout::array::<*mut c_void>(new_pages_len).unwrap();
         let ptr = alloc(layout) as *mut *mut c_void;
         if ptr.is_null() {
             return Err(io::Error::new(io::ErrorKind::Other, "Out of memory"));
         }
         ptr
     };
 
     unsafe {
         memcpy(new_pages as *mut c_void, info.pages.as_ptr() as *const c_void, info.max_pages * mem::size_of::<*mut c_void>());
         if !info.pages.is_empty() {
             dealloc(info.pages.as_mut_ptr() as *mut u8, Layout::array::<*mut c_void>(info.max_pages).unwrap());
         }
     }
 
     info.pages = unsafe { Vec::from_raw_parts(new_pages, new_pages_len, new_pages_len) };
     info.max_pages = new_pages_len;
 
     Ok(())
 }
 
 fn module_get_next_page(info: &mut LoadInfo) -> Result<*mut c_void, io::Error> {
     if info.max_pages == info.used_pages {
         module_extend_max_pages(info, info.used_pages)?;
     }
 
     let page = unsafe { malloc(4096) };
     if page.is_null() {
         return Err(io::Error::new(io::ErrorKind::Other, "Out of memory"));
     }
 
     info.pages[info.used_pages] = page;
     info.used_pages += 1;
 
     Ok(page)
 }
 
 #[cfg(feature = "gzip")]
 fn module_gzip_header_len(buf: &[u8]) -> usize {
     const SIGNATURE: [u8; 3] = [0x1f, 0x8b, 0x08];
     let mut len = 10;
 
     if buf.len() < len || unsafe { memcmp(buf.as_ptr() as *const c_void, SIGNATURE.as_ptr() as *const c_void, SIGNATURE.len()) } != 0 {
         return 0;
     }
 
     if buf[3] & 0x08 != 0 {
         loop {
             if len == buf.len() {
                 return 0;
             }
             if buf[len] == 0 {
                 break;
             }
             len += 1;
         }
     }
 
     len
 }
 
 #[cfg(feature = "gzip")]
 fn module_gzip_decompress(info: &mut LoadInfo, buf: &[u8]) -> Result<ssize_t, io::Error> {
     let mut s = z_stream {
         next_in: ptr::null_mut(),
         avail_in: 0,
         next_out: ptr::null_mut(),
         avail_out: 0,
         msg: ptr::null_mut(),
         state: ptr::null_mut(),
         zalloc: None,
         zfree: None,
         opaque: ptr::null_mut(),
         data_type: 0,
         adler: 0,
         reserved: 0,
     };
     let mut new_size = 0;
     let gzip_hdr_len = module_gzip_header_len(buf);
     if gzip_hdr_len == 0 {
         return Err(io::Error::new(io::ErrorKind::InvalidData, "not a gzip compressed module"));
     }
 
     s.next_in = buf[gzip_hdr_len..].as_ptr() as *mut u8;
     s.avail_in = (buf.len() - gzip_hdr_len) as u32;
 
     let workspace_size = unsafe { zlib_inflate_workspacesize() };
     let workspace = unsafe { malloc(workspace_size) } as *mut c_void;
     if workspace.is_null() {
         return Err(io::Error::new(io::ErrorKind::Other, "Out of memory"));
     }
     s.workspace = workspace;
 
     let rc = unsafe { inflateInit2_(&mut s, -15) };
     if rc != 0 {
         return Err(io::Error::new(io::ErrorKind::Other, format!("failed to initialize decompressor: {}", rc)));
     }
 
     loop {
         let page = module_get_next_page(info)?;
         s.next_out = page as *mut u8;
         s.avail_out = 4096;
         let rc = unsafe { inflate(&mut s, 0) };
         new_size += 4096 - s.avail_out as usize;
         if rc != 0 {
             break;
         }
     }
 
     if rc != 1 {
         return Err(io::Error::new(io::ErrorKind::Other, format!("decompression failed with status {}", rc)));
     }
 
     unsafe { inflateEnd(&mut s) };
     unsafe { free(workspace) };
 
     Ok(new_size as ssize_t)
 }
 
 #[cfg(feature = "xz")]
 fn module_xz_decompress(info: &mut LoadInfo, buf: &[u8]) -> Result<ssize_t, io::Error> {
     const SIGNATURE: [u8; 6] = [0xfd, b'7', b'z', b'X', b'Z', 0];
     let mut xz_dec = xz2::stream::Stream::new_decoder(xz2::stream::Format::Single, 0).unwrap();
     let mut xz_buf = xz2::stream::Buffer::new_decode(buf);
     let mut new_size = 0;
 
     if buf.len() < SIGNATURE.len() || unsafe { memcmp(buf.as_ptr() as *const c_void, SIGNATURE.as_ptr() as *const c_void, SIGNATURE.len()) } != 0 {
         return Err(io::Error::new(io::ErrorKind::InvalidData, "not an xz compressed module"));
     }
 
     loop {
         let page = module_get_next_page(info)?;
         let mut out_buf = unsafe { slice::from_raw_parts_mut(page as *mut u8, 4096) };
         let xz_ret = xz_dec.process(&mut xz_buf, &mut out_buf, Action::Run)?;
         new_size += out_buf.len();
         if xz_ret == xz2::stream::Status::StreamEnd {
             break;
         }
     }
 
     Ok(new_size as ssize_t)
 }
 
 #[cfg(feature = "zstd")]
 fn module_zstd_decompress(info: &mut LoadInfo, buf: &[u8]) -> Result<ssize_t, io::Error> {
     const SIGNATURE: [u8; 4] = [0x28, 0xb5, 0x2f, 0xfd];
     let mut zstd_dec = ZSTD_DCtx::create().unwrap();
     let mut new_size = 0;
 
     if buf.len() < SIGNATURE.len() || unsafe { memcmp(buf.as_ptr() as *const c_void, SIGNATURE.as_ptr() as *const c_void, SIGNATURE.len()) } != 0 {
         return Err(io::Error::new(io::ErrorKind::InvalidData, "not a zstd compressed module"));
     }
 
     loop {
         let page = module_get_next_page(info)?;
         let mut out_buf = unsafe { slice::from_raw_parts_mut(page as *mut u8, 4096) };
         let ret = unsafe { ZSTD_decompressDCtx(zstd_dec, out_buf.as_mut_ptr(), out_buf.len(), buf.as_ptr(), buf.len()) };
         if ret == 0 {
             break;
         }
         new_size += ret as usize;
     }
 
     Ok(new_size as ssize_t)
 }
 
 fn module_decompress(info: &mut LoadInfo, buf: &[u8]) -> Result<(), io::Error> {
     let n_pages = (buf.len() + 4095) / 4096 * 2;
     module_extend_max_pages(info, n_pages)?;
 
     let data_size = match cfg!(feature = "gzip") {
         true => module_gzip_decompress(info, buf)?,
         _ => match cfg!(feature = "xz") {
             true => module_xz_decompress(info, buf)?,
             _ => module_zstd_decompress(info, buf)?,
         },
     };
 
     info.hdr = unsafe { mmap(ptr::null_mut(), info.used_pages * 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0) as *mut c_void };
     if info.hdr.is_null() {
         return Err(io::Error::new(io::ErrorKind::Other, "Out of memory"));
     }
 
     info.len = data_size as usize;
     Ok(())
 }
 
 fn module_decompress_cleanup(info: &mut LoadInfo) {
     if !info.hdr.is_null() {
         unsafe { munmap(info.hdr as *mut c_void, info.used_pages * 4096) };
     }
 
     for page in &info.pages {
         unsafe { free(*page) };
     }
 
     info.pages.clear();
     info.max_pages = 0;
     info.used_pages = 0;
 }
 
 #[cfg(feature = "sysfs")]
 fn compression_show() -> String {
     format!("{}\n", cfg!(feature = "gzip").then_some("gzip").unwrap_or_else(|| cfg!(feature = "xz").then_some("xz").unwrap_or("zstd")))
 }
 
 #[cfg(feature = "sysfs")]
 fn module_decompress_sysfs_init() -> Result<(), io::Error> {
     let kset = Arc::new(Mutex::new(kobject::KSet::new("module_kset")));
     let compression_attr = kobject::KObjAttribute::new("compression", compression_show, None);
     kset.lock().unwrap().add_attribute(compression_attr)?;
     Ok(())
 }
 
 #[cfg(feature = "sysfs")]
 fn late_initcall() {
     module_decompress_sysfs_init().unwrap();
 }
 
 