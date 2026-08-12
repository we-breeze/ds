extern crate proc_macro;

mod ds;

use proc_macro::TokenStream;

#[proc_macro_attribute]
pub fn impl_number_ringslice(args: TokenStream, input: TokenStream) -> TokenStream {
    ds::impl_number_ringslice(args, input)
}
