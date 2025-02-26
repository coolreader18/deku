use crate::macros::{
    gen_ctx_types_and_arg, gen_field_args, gen_struct_destruction, pad_bits, token_contains_string,
    wrap_default_ctx,
};
use crate::{DekuData, DekuDataEnum, DekuDataStruct, FieldData, Id};
use darling::ast::{Data, Fields};
use proc_macro2::TokenStream;
use quote::quote;
use std::convert::TryFrom;

pub(crate) fn emit_deku_write(input: &DekuData) -> Result<TokenStream, syn::Error> {
    match &input.data {
        Data::Enum(_) => emit_enum(input),
        Data::Struct(_) => emit_struct(input),
    }
}

fn emit_struct(input: &DekuData) -> Result<TokenStream, syn::Error> {
    let crate_ = super::get_crate_name();
    let mut tokens = TokenStream::new();

    let DekuDataStruct {
        imp,
        wher,
        ident,
        fields,
    } = DekuDataStruct::try_from(input)?;

    let magic_write = emit_magic_write(input);

    let field_writes = emit_field_writes(input, &fields, None)?;
    let field_updates = emit_field_updates(&fields, Some(quote! { self. }));

    let named = fields.style.is_struct();

    let field_idents = fields
        .iter()
        .enumerate()
        .map(|(i, f)| f.get_ident(i, true))
        .collect::<Vec<_>>();

    let destructured = gen_struct_destruction(named, &input.ident, &field_idents);

    // Implement `DekuContainerWrite` for types that don't need a context
    if input.ctx.is_none() || (input.ctx.is_some() && input.ctx_default.is_some()) {
        let to_bits_body = wrap_default_ctx(
            quote! {
                match *self {
                    #destructured => {
                        let mut __deku_acc: ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8> = ::#crate_::bitvec::BitVec::new();
                        let __deku_output = &mut __deku_acc;

                        #magic_write
                        #(#field_writes)*

                    Ok(__deku_acc)
                    }
                }
            },
            &input.ctx,
            &input.ctx_default,
        );

        tokens.extend(quote! {
            impl #imp core::convert::TryFrom<#ident> for ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8> #wher {
                type Error = ::#crate_::DekuError;

                fn try_from(input: #ident) -> Result<Self, Self::Error> {
                    input.to_bits()
                }
            }

            impl #imp core::convert::TryFrom<#ident> for Vec<u8> #wher {
                type Error = ::#crate_::DekuError;

                fn try_from(input: #ident) -> Result<Self, Self::Error> {
                    input.to_bytes()
                }
            }

            impl #imp DekuContainerWrite for #ident #wher {
                fn to_bytes(&self) -> Result<Vec<u8>, ::#crate_::DekuError> {
                    let mut acc: ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8> = self.to_bits()?;
                    Ok(acc.into_vec())
                }

                #[allow(unused_variables)]
                fn to_bits(&self) -> Result<::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8>, ::#crate_::DekuError> {
                    #to_bits_body
                }
            }
        });
    }

    let (ctx_types, ctx_arg) = gen_ctx_types_and_arg(input.ctx.as_ref())?;

    let write_body = quote! {
        match *self {
            #destructured => {
                #magic_write
                #(#field_writes)*

                Ok(())
            }
        }
    };

    // avoid outputing `use core::convert::TryInto` if update() function is empty
    let update_use = check_update_use(&field_updates);

    tokens.extend(quote! {
        impl #imp DekuUpdate for #ident #wher {
            fn update(&mut self) -> Result<(), ::#crate_::DekuError> {
                #update_use
                #(#field_updates)*

                Ok(())
            }
        }

        impl #imp DekuWrite<#ctx_types> for #ident #wher {
            #[allow(unused_variables)]
            fn write(&self, __deku_output: &mut ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8>, #ctx_arg) -> Result<(), ::#crate_::DekuError> {
                #write_body
            }
        }
    });

    if input.ctx.is_some() && input.ctx_default.is_some() {
        let write_body = wrap_default_ctx(write_body, &input.ctx, &input.ctx_default);

        tokens.extend(quote! {
            impl #imp DekuWrite for #ident #wher {
                #[allow(unused_variables)]
                fn write(&self, __deku_output: &mut ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8>, _: ()) -> Result<(), ::#crate_::DekuError> {
                    #write_body
                }
            }
        });
    }

    // println!("{}", tokens.to_string());
    Ok(tokens)
}

fn emit_enum(input: &DekuData) -> Result<TokenStream, syn::Error> {
    let crate_ = super::get_crate_name();
    let mut tokens = TokenStream::new();

    let DekuDataEnum {
        imp,
        wher,
        variants,
        ident,
        id,
        id_type,
        id_args,
    } = DekuDataEnum::try_from(input)?;

    let magic_write = emit_magic_write(input);

    let mut variant_writes = vec![];
    let mut variant_updates = vec![];

    let has_discriminant = variants.iter().any(|v| v.discriminant.is_some());

    for variant in variants {
        // check if the first field has an ident, if not, it's a unnamed struct
        let variant_is_named = variant
            .fields
            .fields
            .get(0)
            .and_then(|v| v.ident.as_ref())
            .is_some();

        let variant_ident = &variant.ident;
        let variant_writer = &variant.writer;

        let field_idents = variant
            .fields
            .as_ref()
            .iter()
            .enumerate()
            .map(|(i, f)| f.get_ident(i, true))
            .collect::<Vec<_>>();

        let variant_id_write = if id.is_some() {
            quote! {
                // if we don't do this we may get a "unused variable" error if passed via `ctx`
                // i.e. #[deku(ctx = "my_id: u8", id = "my_id")]
                let _ = #id;
            }
        } else if id_type.is_some() {
            if let Some(variant_id) = &variant.id {
                match variant_id {
                    Id::TokenStream(v) => {
                        quote! {
                            let mut __deku_variant_id: #id_type = #v;
                            __deku_variant_id.write(__deku_output, (#id_args))?;
                        }
                    }
                    Id::LitByteStr(v) => {
                        quote! {
                            let mut __deku_variant_id: #id_type = *#v;
                            __deku_variant_id.write(__deku_output, (#id_args))?;
                        }
                    }
                }
            } else if variant.id_pat.is_some() {
                quote! {}
            } else if has_discriminant {
                quote! {
                    let mut __deku_variant_id: #id_type = Self::#variant_ident as #id_type;
                    __deku_variant_id.write(__deku_output, (#id_args))?;
                }
            } else {
                return Err(syn::Error::new(
                    variant.ident.span(),
                    "DekuWrite: `id` must be specified on non-unit variants",
                ));
            }
        } else {
            // either `id` or `type` needs to be specified
            unreachable!();
        };

        let variant_match = super::gen_enum_init(variant_is_named, variant_ident, field_idents);

        let variant_write = if variant_writer.is_some() {
            quote! { #variant_writer ?; }
        } else {
            let field_writes = emit_field_writes(input, &variant.fields.as_ref(), None)?;

            quote! {
                {
                    #variant_id_write
                    #(#field_writes)*
                }
            }
        };

        let variant_field_updates = emit_field_updates(&variant.fields.as_ref(), None);

        variant_writes.push(quote! {
            Self :: #variant_match => {
                #variant_write
            }
        });

        variant_updates.push(quote! {
            Self :: #variant_match => {
                #(#variant_field_updates)*
            }
        });
    }

    // Implement `DekuContainerWrite` for types that don't need a context
    if input.ctx.is_none() || (input.ctx.is_some() && input.ctx_default.is_some()) {
        let to_bits_body = wrap_default_ctx(
            quote! {
                let mut __deku_acc: ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8> = ::#crate_::bitvec::BitVec::new();
                let __deku_output = &mut __deku_acc;

                #magic_write

                match self {
                    #(#variant_writes),*
                }

                Ok(__deku_acc)
            },
            &input.ctx,
            &input.ctx_default,
        );

        tokens.extend(quote! {
            impl #imp core::convert::TryFrom<#ident> for ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8> #wher {
                type Error = ::#crate_::DekuError;

                fn try_from(input: #ident) -> Result<Self, Self::Error> {
                    input.to_bits()
                }
            }

            impl #imp core::convert::TryFrom<#ident> for Vec<u8> #wher {
                type Error = ::#crate_::DekuError;

                fn try_from(input: #ident) -> Result<Self, Self::Error> {
                    input.to_bytes()
                }
            }

            impl #imp DekuContainerWrite for #ident #wher {
                fn to_bytes(&self) -> Result<Vec<u8>, ::#crate_::DekuError> {
                    let mut acc: ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8> = self.to_bits()?;
                    Ok(acc.into_vec())
                }

                #[allow(unused_variables)]
                fn to_bits(&self) -> Result<::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8>, ::#crate_::DekuError> {
                    #to_bits_body
                }
            }
        })
    }

    let (ctx_types, ctx_arg) = gen_ctx_types_and_arg(input.ctx.as_ref())?;

    let write_body = quote! {
        #magic_write

        match self {
            #(#variant_writes),*
        }

        Ok(())
    };

    // avoid outputting `use core::convert::TryInto` if update() function is empty
    let update_use = check_update_use(&variant_updates);

    tokens.extend(quote! {
        impl #imp DekuUpdate for #ident #wher {
            fn update(&mut self) -> Result<(), ::#crate_::DekuError> {
                #update_use

                match self {
                    #(#variant_updates),*
                }

                Ok(())
            }
        }

        impl #imp DekuWrite<#ctx_types> for #ident #wher {
            #[allow(unused_variables)]
            fn write(&self, __deku_output: &mut ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8>, #ctx_arg) -> Result<(), ::#crate_::DekuError> {
                #write_body
            }
        }
    });

    if input.ctx.is_some() && input.ctx_default.is_some() {
        let write_body = wrap_default_ctx(write_body, &input.ctx, &input.ctx_default);

        tokens.extend(quote! {
            impl #imp DekuWrite for #ident #wher {
                #[allow(unused_variables)]
                fn write(&self, __deku_output: &mut ::#crate_::bitvec::BitVec<::#crate_::bitvec::Msb0, u8>, _: ()) -> Result<(), ::#crate_::DekuError> {
                    #write_body
                }
            }
        });
    }

    // println!("{}", tokens.to_string());
    Ok(tokens)
}

fn emit_magic_write(input: &DekuData) -> TokenStream {
    if let Some(magic) = &input.magic {
        quote! {
            #magic.write(__deku_output, ())?;
        }
    } else {
        quote! {}
    }
}

fn emit_field_writes(
    input: &DekuData,
    fields: &Fields<&FieldData>,
    object_prefix: Option<TokenStream>,
) -> Result<Vec<TokenStream>, syn::Error> {
    fields
        .iter()
        .enumerate()
        .map(|(i, f)| emit_field_write(input, i, f, &object_prefix))
        .collect()
}

fn emit_field_updates(
    fields: &Fields<&FieldData>,
    object_prefix: Option<TokenStream>,
) -> Vec<TokenStream> {
    let mut field_updates = vec![];

    for (i, f) in fields.iter().enumerate() {
        let new_field_updates = emit_field_update(i, f, &object_prefix);
        field_updates.extend(new_field_updates);
    }

    field_updates
}

fn emit_field_update(
    i: usize,
    f: &FieldData,
    object_prefix: &Option<TokenStream>,
) -> Vec<TokenStream> {
    let mut field_updates = vec![];

    let field_ident = f.get_ident(i, object_prefix.is_none());
    let deref = if object_prefix.is_none() {
        Some(quote! { * })
    } else {
        None
    };

    if let Some(field_update) = &f.update {
        field_updates.push(quote! {
            #deref #object_prefix #field_ident = (#field_update).try_into()?;
        })
    }

    field_updates
}

fn emit_bit_byte_offsets(
    fields: &[&Option<TokenStream>],
) -> (Option<TokenStream>, Option<TokenStream>) {
    // determine if we should include `bit_offset` and `byte_offset`
    let byte_offset = if fields
        .iter()
        .any(|v| token_contains_string(v, "__deku_byte_offset"))
    {
        Some(quote! {
            let __deku_byte_offset = __deku_bit_offset / 8;
        })
    } else {
        None
    };

    let bit_offset = if fields
        .iter()
        .any(|v| token_contains_string(v, "__deku_bit_offset"))
        || byte_offset.is_some()
    {
        Some(quote! {
            let __deku_bit_offset = __deku_output.len();
        })
    } else {
        None
    };

    (bit_offset, byte_offset)
}

fn emit_padding(bit_size: &TokenStream) -> TokenStream {
    let crate_ = super::get_crate_name();
    quote! {
        {
            use core::convert::TryFrom;
            let __deku_pad = usize::try_from(#bit_size).map_err(|e|
                ::#crate_::DekuError::InvalidParam(format!(
                    "Invalid padding param \"({})\": cannot convert to usize",
                    stringify!(#bit_size)
                ))
            )?;
            let new_len = __deku_output.len() + __deku_pad;
            __deku_output.resize(new_len, false);
        }
    }
}

fn emit_field_write(
    input: &DekuData,
    i: usize,
    f: &FieldData,
    object_prefix: &Option<TokenStream>,
) -> Result<TokenStream, syn::Error> {
    let crate_ = super::get_crate_name();
    let field_endian = f.endian.as_ref().or_else(|| input.endian.as_ref());

    // fields to check usage of bit/byte offset
    let field_check_vars = [
        &f.writer,
        &f.cond,
        &f.ctx.as_ref().map(|v| quote!(#v)),
        &f.assert,
        &f.assert_eq,
    ];

    let (bit_offset, byte_offset) = emit_bit_byte_offsets(&field_check_vars);

    let field_writer = &f.writer;
    let field_ident = f.get_ident(i, object_prefix.is_none());
    let field_ident_str = field_ident.to_string();

    let field_assert = f.assert.as_ref().map(|v| {
        quote! {
            if (!(#v)) {
                // assertion is false, raise error
                return Err(::#crate_::DekuError::Assertion(format!(
                            "field '{}' failed assertion: {}",
                            #field_ident_str,
                            stringify!(#v)
                        )));
            } else {
                // do nothing
            }
        }
    });

    let field_assert_eq = f.assert_eq.as_ref().map(|v| {
        quote! {
            if (!(*(#field_ident) == (#v))) {
                // assertion is false, raise error
                return Err(::#crate_::DekuError::Assertion(format!(
                            "field '{}' failed assertion: {}",
                            #field_ident_str,
                            stringify!(#field_ident == #v)
                        )));
            } else {
                // do nothing
            }
        }
    });

    let field_write_func = if field_writer.is_some() {
        quote! { #field_writer }
    } else {
        let write_args = gen_field_args(
            field_endian,
            f.bits.as_ref(),
            f.bytes.as_ref(),
            f.ctx.as_ref(),
        )?;

        quote! { #object_prefix #field_ident.write(__deku_output, (#write_args)) }
    };

    let pad_bits_before = pad_bits(
        f.pad_bits_before.as_ref(),
        f.pad_bytes_before.as_ref(),
        emit_padding,
    );
    let pad_bits_after = pad_bits(
        f.pad_bits_after.as_ref(),
        f.pad_bytes_after.as_ref(),
        emit_padding,
    );

    let field_write_normal = quote! {
        #field_write_func ?;
    };

    let field_write_tokens = match (f.skip, &f.cond) {
        (true, Some(field_cond)) => {
            // #[deku(skip, cond = "...")] ==> `skip` if `cond`
            quote! {
                if (#field_cond) {
                   // skipping, no write
                } else {
                    #field_write_normal
                }
            }
        }
        (true, None) => {
            // #[deku(skip)] ==> `skip`
            quote! {
                // skipping, no write
            }
        }
        (false, _) => {
            quote! {
                #field_write_normal
            }
        }
    };

    let field_write = quote! {
        #pad_bits_before

        #bit_offset
        #byte_offset

        #field_assert
        #field_assert_eq

        #field_write_tokens

        #pad_bits_after
    };

    Ok(field_write)
}

/// avoid outputing `use core::convert::TryInto` if update() function is generated with empty Vec
fn check_update_use<T>(vec: &[T]) -> TokenStream {
    if !vec.is_empty() {
        quote! {use core::convert::TryInto;}
    } else {
        quote! {}
    }
}
