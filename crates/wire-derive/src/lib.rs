//! `#[derive(Wire)]`: the single interpreter of a message type's definition (hecate's rule,
//! [C: survey-hecate.md §4.6]). For a struct it emits a canonical encoder that writes the fields
//! in declaration order and a decoder that reads them back, refusing anything non-canonical; for
//! an enum it emits a `u32` discriminant (the variant's index) followed by the variant's fields.
//! It also emits the type's reflection (`SCHEMA`, a canonical text such as
//! `struct Name{a:u32,b:Vec<Item>}`) and `SCHEMA_HASH`, a compile-time structural hash of the
//! reflection mixed with every field type's own hash, so a change anywhere in the tree changes
//! the hash (see `slates_wire::schema`).
//!
//! Refusals are compile errors with the field named: a type without a `Wire` implementation, a
//! generic type, a union, a tuple struct, `usize`/`isize` (which do not exist on the wire).

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, Type, parse_macro_input, spanned::Spanned};

/// Derives `slates_wire::Wire` for a struct with named fields or an enum whose variants have
/// named fields or none.
#[proc_macro_derive(Wire)]
pub fn derive_wire(input: TokenStream) -> TokenStream {
  let input = parse_macro_input!(input as DeriveInput);
  match expand(&input) {
    Ok(tokens) => tokens.into(),
    Err(e) => e.to_compile_error().into(),
  }
}

fn expand(input: &DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
  if !input.generics.params.is_empty() {
    return Err(syn::Error::new(
      input.generics.span(),
      "Wire: generic types have no single schema; write a concrete type",
    ));
  }
  let name = &input.ident;
  let name_text = name.to_string();
  match &input.data {
    Data::Struct(s) => expand_struct(name, &name_text, &s.fields),
    Data::Enum(e) => expand_enum(name, &name_text, e),
    Data::Union(u) => Err(syn::Error::new(
      u.union_token.span(),
      "Wire: a union has no canonical encoding",
    )),
  }
}

fn named(fields: &Fields, what: &str) -> syn::Result<Vec<(syn::Ident, Type)>> {
  match fields {
    Fields::Named(named) => named
      .named
      .iter()
      .map(|f| {
        let ident = f
          .ident
          .clone()
          .ok_or_else(|| syn::Error::new(f.span(), "Wire: a field must be named"))?;
        check_type(&f.ty)?;
        Ok((ident, f.ty.clone()))
      })
      .collect(),
    Fields::Unit => Ok(Vec::new()),
    Fields::Unnamed(u) => Err(syn::Error::new(
      u.span(),
      format!(
        "Wire: {what} must use named fields (tuple fields have no stable names in the schema)"
      ),
    )),
  }
}

fn check_type(ty: &Type) -> syn::Result<()> {
  let text = quote!(#ty).to_string().replace(' ', "");
  if text == "usize" || text == "isize" || text.contains("<usize>") || text.contains("<isize>") {
    return Err(syn::Error::new(
      ty.span(),
      "Wire: usize and isize do not exist on the wire; use u32 or u64",
    ));
  }
  Ok(())
}

fn type_text(ty: &Type) -> String {
  quote!(#ty).to_string().replace(' ', "")
}

fn expand_struct(
  name: &syn::Ident,
  name_text: &str,
  fields: &Fields,
) -> syn::Result<proc_macro2::TokenStream> {
  let fields = named(fields, "a struct")?;
  let idents: Vec<&syn::Ident> = fields.iter().map(|(i, _)| i).collect();
  let types: Vec<&Type> = fields.iter().map(|(_, t)| t).collect();
  let reflection = format!(
    "struct {name_text}{{{}}}",
    fields
      .iter()
      .map(|(i, t)| format!("{i}:{}", type_text(t)))
      .collect::<Vec<_>>()
      .join(",")
  );
  Ok(quote! {
    impl ::slates_wire::Wire for #name {
      const SCHEMA: &'static str = #reflection;
      const SCHEMA_HASH: u64 = ::slates_wire::schema::mix(
        ::slates_wire::schema::fnv64(#reflection),
        &[#(<#types as ::slates_wire::Wire>::SCHEMA_HASH),*],
      );
      fn encode(&self, out: &mut ::std::vec::Vec<u8>) {
        #(::slates_wire::Wire::encode(&self.#idents, out);)*
      }
      fn decode(input: &mut &[u8]) -> ::std::result::Result<Self, ::slates_wire::WireError> {
        Ok(Self { #(#idents: <#types as ::slates_wire::Wire>::decode(input)?,)* })
      }
    }
  })
}

/// One variant's contribution to the enum's impl.
struct VariantParts {
  reflection: String,
  encode_arm: proc_macro2::TokenStream,
  decode_arm: proc_macro2::TokenStream,
  hashes: Vec<proc_macro2::TokenStream>,
}

fn expand_variant(index: usize, variant: &syn::Variant) -> syn::Result<VariantParts> {
  let discriminant =
    u32::try_from(index).map_err(|_| syn::Error::new(variant.span(), "Wire: too many variants"))?;
  let vname = &variant.ident;
  let fields = named(&variant.fields, "a variant")?;
  let idents: Vec<&syn::Ident> = fields.iter().map(|(i, _)| i).collect();
  let types: Vec<&Type> = fields.iter().map(|(_, t)| t).collect();
  let bound: Vec<syn::Ident> = idents
    .iter()
    .map(|i| format_ident!("field_{}", i))
    .collect();
  let reflection = format!(
    "{vname}{{{}}}",
    fields
      .iter()
      .map(|(i, t)| format!("{i}:{}", type_text(t)))
      .collect::<Vec<_>>()
      .join(",")
  );
  let hashes = types
    .iter()
    .map(|t| quote!(<#t as ::slates_wire::Wire>::SCHEMA_HASH))
    .collect();
  let (encode_arm, decode_arm) = if fields.is_empty() {
    unit_arms(vname, discriminant)
  } else {
    field_arms(vname, discriminant, &idents, &types, &bound)
  };
  Ok(VariantParts {
    reflection,
    encode_arm,
    decode_arm,
    hashes,
  })
}

fn unit_arms(
  vname: &syn::Ident,
  discriminant: u32,
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
  (
    quote! { Self::#vname => { ::slates_wire::Wire::encode(&#discriminant, out); } },
    quote! { #discriminant => Ok(Self::#vname), },
  )
}

fn field_arms(
  vname: &syn::Ident,
  discriminant: u32,
  idents: &[&syn::Ident],
  types: &[&Type],
  bound: &[syn::Ident],
) -> (proc_macro2::TokenStream, proc_macro2::TokenStream) {
  (
    quote! {
      Self::#vname { #(#idents: #bound),* } => {
        ::slates_wire::Wire::encode(&#discriminant, out);
        #(::slates_wire::Wire::encode(#bound, out);)*
      }
    },
    quote! {
      #discriminant => Ok(Self::#vname { #(#idents: <#types as ::slates_wire::Wire>::decode(input)?,)* }),
    },
  )
}

fn expand_enum(
  name: &syn::Ident,
  name_text: &str,
  data: &syn::DataEnum,
) -> syn::Result<proc_macro2::TokenStream> {
  let parts = data
    .variants
    .iter()
    .enumerate()
    .map(|(index, variant)| expand_variant(index, variant))
    .collect::<syn::Result<Vec<_>>>()?;
  let reflection = format!(
    "enum {name_text}{{{}}}",
    parts
      .iter()
      .map(|p| p.reflection.as_str())
      .collect::<Vec<_>>()
      .join(",")
  );
  let encode_arms = parts.iter().map(|p| &p.encode_arm);
  let decode_arms = parts.iter().map(|p| &p.decode_arm);
  let hashes = parts.iter().flat_map(|p| p.hashes.iter());
  Ok(quote! {
    impl ::slates_wire::Wire for #name {
      const SCHEMA: &'static str = #reflection;
      const SCHEMA_HASH: u64 = ::slates_wire::schema::mix(
        ::slates_wire::schema::fnv64(#reflection),
        &[#(#hashes),*],
      );
      fn encode(&self, out: &mut ::std::vec::Vec<u8>) {
        match self { #(#encode_arms)* }
      }
      fn decode(input: &mut &[u8]) -> ::std::result::Result<Self, ::slates_wire::WireError> {
        let discriminant = <u32 as ::slates_wire::Wire>::decode(input)?;
        match discriminant {
          #(#decode_arms)*
          other => Err(::slates_wire::WireError::BadDiscriminant { got: other }),
        }
      }
    }
  })
}
