use std::{
    alloc::{
        Layout,
        LayoutError,
        alloc,
        dealloc,
    },
    ptr::NonNull,
};

use tracing::info;

/// A aligned vec to page size
#[derive(Debug)]
pub struct PageAlignedBuffer {
    pointer: NonNull<u8>,
    layout: Layout,
}
impl PageAlignedBuffer {
    /// construct with size
    pub fn new(size: usize) -> Result<Self, LayoutError> {
        let page_size = rustix::param::page_size();
        info!("page size: {}", page_size);
        let size_with_pages = page_size * size;
        let layout = Layout::from_size_align(size_with_pages, page_size)?;
        //Safety: we construct a allocation with pages sizes from os, this is why it can never be null
        let pointer = unsafe { alloc(layout) };
        let pointer = NonNull::new(pointer).expect("Pointert error");
        Ok(Self { pointer, layout })
    }
    /// Returns the aligned pointer
    pub fn as_ptr(&self) -> *mut u8 {
        self.pointer.as_ptr()
    }

    /// Returns the usable size
    pub fn size(&self) -> usize {
        self.layout.size()
    }
}
/// get buffer and size
pub fn page_aligned_buffer(pages: usize) -> (PageAlignedBuffer, *mut u8, usize) {
    let buffer = PageAlignedBuffer::new(pages).expect("Failed");
    let ptr = buffer.as_ptr();
    let size = buffer.size();

    (buffer, ptr, size)
}
// Automatically handle deallocation (RAII)
impl Drop for PageAlignedBuffer {
    fn drop(&mut self) {
        unsafe {
            dealloc(self.pointer.as_ptr(), self.layout);
        }
    }
}
unsafe impl Send for PageAlignedBuffer {}
unsafe impl Sync for PageAlignedBuffer {}
