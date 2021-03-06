// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;

use syn::{
    parse_quote, punctuated::Punctuated, GenericArgument, PathArguments, PathSegment, Type,
    TypePath, TypePtr, TypeReference,
};

use crate::{
    conversion::bridge_converter::ConvertError,
    known_types::{is_known_type, known_type_substitute_path, should_dereference_in_cpp},
    types::{Namespace, TypeName},
};

use super::typedef_analyzer::{TypedefTarget, analyze_typedef_target};

pub(crate) struct TypeConverter {
    types_found: Vec<TypeName>,
    typedefs: HashMap<TypeName, TypedefTarget>,
}

impl TypeConverter {
    pub(crate) fn new() -> Self {
        Self {
            types_found: Vec::new(),
            typedefs: HashMap::new(),
        }
    }

    pub(crate) fn push(&mut self, ty: TypeName) {
        self.types_found.push(ty);
    }

    pub(crate) fn insert_typedef(&mut self, id: TypeName, ty: &Type) {
        let target = analyze_typedef_target(ty);
        self.typedefs.insert(id, target);
    }

    pub(crate) fn convert_boxed_type(
        &self,
        ty: Box<Type>,
        ns: &Namespace,
    ) -> Result<Box<Type>, ConvertError> {
        Ok(Box::new(self.convert_type(*ty, ns)?))
    }

    fn convert_type(&self, ty: Type, ns: &Namespace) -> Result<Type, ConvertError> {
        let result = match ty {
            Type::Path(p) => {
                let newp = self.convert_type_path(p, ns)?;
                // Special handling because rust_Str (as emitted by bindgen)
                // doesn't simply get renamed to a different type _identifier_.
                // This plain type-by-value (as far as bindgen is concerned)
                // is actually a &str.
                if should_dereference_in_cpp(&newp) {
                    Type::Reference(parse_quote! {
                        &str
                    })
                } else {
                    Type::Path(newp)
                }
            }
            Type::Reference(mut r) => {
                r.elem = self.convert_boxed_type(r.elem, ns)?;
                Type::Reference(r)
            }
            Type::Ptr(ptr) => Type::Reference(self.convert_ptr_to_reference(ptr, ns)?),
            _ => ty,
        };
        Ok(result)
    }

    fn convert_type_path(
        &self,
        mut typ: TypePath,
        ns: &Namespace,
    ) -> Result<TypePath, ConvertError> {
        if typ.path.segments.iter().next().unwrap().ident == "root" {
            typ.path.segments = typ
                .path
                .segments
                .into_iter()
                .map(|s| -> Result<PathSegment, ConvertError> {
                    let ident = &s.ident;
                    let args = match s.arguments {
                        PathArguments::AngleBracketed(mut ab) => {
                            ab.args = self.convert_punctuated(ab.args, ns)?;
                            PathArguments::AngleBracketed(ab)
                        }
                        _ => s.arguments,
                    };
                    Ok(parse_quote!( #ident #args ))
                })
                .collect::<Result<_, _>>()?;
        } else {
            let ty = TypeName::from_type_path(&typ);
            // If the type looks like it is unqualified, check we know it
            // already, and if not, qualify it according to the current
            // namespace. This is a bit of a shortcut compared to having a full
            // resolution pass which can search all known namespaces.
            if !self.types_found.contains(&ty) && !is_known_type(&ty) {
                typ.path.segments = std::iter::once(&"root".to_string())
                    .chain(ns.iter())
                    .map(|s| parse_quote! { #s })
                    .chain(typ.path.segments.into_iter())
                    .collect();
            }
        }
        let mut last_seg_args = None;
        let mut seg_iter = typ.path.segments.iter().peekable();
        while let Some(seg) = seg_iter.next() {
            if !seg.arguments.is_empty() {
                if seg_iter.peek().is_some() {
                    panic!("Did not expect bindgen to create a type with path arguments on a non-final segment")
                } else {
                    last_seg_args = Some(seg.arguments.clone());
                }
            }
        }
        drop(seg_iter);
        let tn = TypeName::from_type_path(&typ);
        // Let's see if this is a typedef.
        let typ = self
            .resolve_typedef(&tn)?
            .map(|x| x.to_type_path())
            .unwrap_or(typ);

        // This will strip off any path arguments...
        let mut typ = known_type_substitute_path(&typ).unwrap_or(typ);
        // but then we'll put them back again as necessary.
        if let Some(last_seg_args) = last_seg_args {
            let last_seg = typ.path.segments.last_mut().unwrap();
            last_seg.arguments = last_seg_args;
        }
        Ok(typ)
    }

    fn convert_punctuated<P>(
        &self,
        pun: Punctuated<GenericArgument, P>,
        ns: &Namespace,
    ) -> Result<Punctuated<GenericArgument, P>, ConvertError>
    where
        P: Default,
    {
        let mut new_pun = Punctuated::new();
        for arg in pun.into_iter() {
            new_pun.push(match arg {
                GenericArgument::Type(t) => GenericArgument::Type(self.convert_type(t, ns)?),
                _ => arg,
            });
        }
        Ok(new_pun)
    }

    fn resolve_typedef<'b>(
        &'b self,
        tn: &'b TypeName,
    ) -> Result<Option<&'b TypeName>, ConvertError> {
        match self.typedefs.get(&tn) {
            None => Ok(None),
            Some(TypedefTarget::NoArguments(original_tn)) => {
                match self.resolve_typedef(original_tn)? {
                    None => Ok(Some(original_tn)),
                    Some(further_resolution) => Ok(Some(further_resolution)),
                }
            }
            _ => Err(ConvertError::ComplexTypedefTarget(tn.to_cpp_name())),
        }
    }

    fn convert_ptr_to_reference(
        &self,
        ptr: TypePtr,
        ns: &Namespace,
    ) -> Result<TypeReference, ConvertError> {
        let mutability = ptr.mutability;
        let elem = self.convert_boxed_type(ptr.elem, ns)?;
        // TODO - in the future, we should check if this is a rust::Str and throw
        // a wobbler if not. rust::Str should only be seen _by value_ in C++
        // headers; it manifests as &str in Rust but on the C++ side it must
        // be a plain value. We should detect and abort.
        Ok(parse_quote! {
            & #mutability #elem
        })
    }
}
