pub mod site_c;
pub mod site_d;

/// Padding to prevent false sharing between atomic variables
#[repr(align(64))]
pub(crate) struct CachePadded<T> {
    value: T,
}

impl<T> CachePadded<T> {
    fn new(value: T) -> Self {
        Self { value }
    }
}
