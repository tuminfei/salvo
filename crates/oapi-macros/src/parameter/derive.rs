use std::borrow::Cow;

use proc_macro2::{Span, TokenStream};
use proc_macro_error::abort;
use quote::{quote, ToTokens};
use syn::{
    parse::Parse, punctuated::Punctuated, Attribute, Data, Field, GenericParam, Generics, Ident, Lifetime,
    LifetimeParam, Token,
};

use crate::component::{self, ComponentSchema};
use crate::doc_comment::CommentAttributes;
use crate::feature::{
    self, impl_into_inner, impl_merge, parse_features, pop_feature, pop_feature_as_inner, AdditionalProperties,
    AllowReserved, DefaultStyle, Example, ExclusiveMaximum, ExclusiveMinimum, Explode, Feature, FeaturesExt, Format,
    Inline, IntoInner, MaxItems, MaxLength, Maximum, Merge, MinItems, MinLength, Minimum, MultipleOf, Names, Nullable,
    Pattern, ReadOnly, Rename, RenameAll, SchemaWith, Style, ToTokensExt, WriteOnly, XmlAttr,
};
use crate::parameter::ParameterIn;
use crate::serde_util::{self, RenameRule, SerdeContainer, SerdeValue};
use crate::type_tree::TypeTree;
use crate::{attribute, Array, FieldRename, Required, ResultExt};

impl_merge!(ToParametersFeatures, FieldFeatures);

/// Container attribute `#[salvo(parameters(...))]`.
pub(crate) struct ToParametersFeatures(Vec<Feature>);

impl Parse for ToParametersFeatures {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        Ok(Self(parse_features!(
            input as DefaultStyle,
            feature::DefaultParameterIn,
            Names,
            RenameAll
        )))
    }
}

impl_into_inner!(ToParametersFeatures);

#[derive(Debug)]
pub(crate) struct ToParameters {
    /// Attributes tagged on the whole struct or enum.
    pub(crate) attrs: Vec<Attribute>,
    /// Generics required to complete the definition.
    pub(crate) generics: Generics,
    /// Data within the struct or enum.
    pub(crate) data: Data,
    /// Name of the struct or enum.
    pub(crate) ident: Ident,
}

impl ToTokens for ToParameters {
    fn to_tokens(&self, tokens: &mut proc_macro2::TokenStream) {
        let ident = &self.ident;
        let salvo = crate::salvo_crate();
        let oapi = crate::oapi_crate();
        let (impl_generics, ty_generics, where_clause) = self.generics.split_for_impl();

        let ex_life = &Lifetime::new("'__macro_gen_ex", Span::call_site());
        let ex_lifetime: GenericParam = LifetimeParam::new(ex_life.clone()).into();
        let mut ex_generics = self.generics.clone();
        ex_generics.params.insert(0, ex_lifetime);
        let ex_impl_generics = ex_generics.split_for_impl().0;

        let mut parameters_features = self
            .attrs
            .iter()
            .filter(|attr| attr.path().is_ident("salvo"))
            .filter_map(|attr| attribute::find_nested_list(attr, "parameters").ok().flatten())
            .map(|meta| meta.parse_args::<ToParametersFeatures>().unwrap_or_abort().into_inner())
            .reduce(|acc, item| acc.merge(item));
        let serde_container = serde_util::parse_container(&self.attrs);

        // #[param] is only supported over fields
        if self.attrs.iter().any(|attr| {
            attr.path().is_ident("salvo") && attribute::find_nested_list(attr, "parameter").ok().flatten().is_some()
        }) {
            abort! {
                ident,
                "found `parameter` attribute in unsupported context";
                help = "Did you mean `parameters`?",
            }
        }

        let names = parameters_features.as_mut().and_then(|features| {
            features
                .pop_by(|feature| matches!(feature, Feature::ToParametersNames(_)))
                .and_then(|feature| match feature {
                    Feature::ToParametersNames(names) => Some(names.into_values()),
                    _ => None,
                })
        });

        let default_style = pop_feature!(parameters_features => Feature::DefaultStyle(_));
        let default_parameter_in = pop_feature!(parameters_features => Feature::DefaultParameterIn(_));
        let rename_all = pop_feature!(parameters_features => Feature::RenameAll(_));
        let default_source_from =
            if let Some(Feature::DefaultParameterIn(feature::DefaultParameterIn(default_parameter_in))) =
                default_parameter_in
            {
                match default_parameter_in {
                    ParameterIn::Query => quote! { #salvo::extract::metadata::SourceFrom::Query },
                    ParameterIn::Header => quote! { #salvo::extract::metadata::SourceFrom::Header },
                    ParameterIn::Path => quote! { #salvo::extract::metadata::SourceFrom::Param },
                    ParameterIn::Cookie => quote! { #salvo::extract::metadata::SourceFrom::Cookie },
                }
            } else {
                quote! { #salvo::extract::metadata::SourceFrom::Query }
            };
        let default_source = quote! { #salvo::extract::metadata::Source::new(#default_source_from, #salvo::extract::metadata::SourceParser::MultiMap) };
        let params = self
            .get_struct_fields(&names.as_ref())
            .enumerate()
            .filter_map(|(index, field)| {
                let field_serde_params = serde_util::parse_value(&field.attrs);
                if matches!(&field_serde_params, Some(params) if !params.skip) {
                    Some((index, field, field_serde_params))
                } else {
                    None
                }
            })
            .map(|(index, field, field_serde_params)|{
                Parameter {
                    field,
                    field_serde_params,
                    container_attributes: FieldParameterContainerAttributes {
                        rename_all: rename_all.as_ref().and_then(|feature| {
                            match feature {
                                Feature::RenameAll(rename_all) => Some(rename_all),
                                _ => None
                            }
                        }),
                        default_style: &default_style,
                        default_parameter_in: &default_parameter_in,
                        name: names.as_ref()
                            .map(|names| names.get(index).unwrap_or_else(|| abort!(
                                ident,
                                "There is no name specified in the names(...) container attribute for tuple struct field {}",
                                index
                            ))),
                    },
                    serde_container: serde_container.as_ref(),
                }
            })
            .collect::<Array<Parameter>>();

        let extract_fields = if self.is_named_struct() {
            params
                .iter()
                .map(|param| param.to_extract_field_token_stream(&salvo))
                .collect::<Vec<_>>()
        } else if let Some(names) = &names {
            names
                .iter()
                .map(|name| quote! { #salvo::extract::metadata::Field::new(#name)})
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        fn quote_rename_rule(salvo: &Ident, rename_all: &RenameRule) -> TokenStream {
            let rename_all = match rename_all {
                RenameRule::LowerCase => "LowerCase",
                RenameRule::UpperCase => "UpperCase",
                RenameRule::PascalCase => "PascalCase",
                RenameRule::CamelCase => "CamelCase",
                RenameRule::SnakeCase => "SnakeCase",
                RenameRule::ScreamingSnakeCase => "ScreamingSnakeCase",
                RenameRule::KebabCase => "KebabCase",
                RenameRule::ScreamingKebabCase => "ScreamingKebabCase",
            };
            let rule = Ident::new(&rename_all, Span::call_site());
            quote! {
                #salvo::extract::RenameRule::#rule
            }
        }
        let rename_all = rename_all
            .as_ref()
            .map(|feature| match feature {
                Feature::RenameAll(RenameAll(rename_rule)) => {
                    let rule = quote_rename_rule(&salvo, rename_rule);
                    Some(quote! {
                        .rename_all(#rule)
                    })
                }
                _ => None,
            })
            .unwrap_or_else(|| None);
        let serde_rename_all =
            if let Some(serde_rename_all) = serde_container.as_ref().and_then(|container| container.rename_all) {
                let rule = quote_rename_rule(&salvo, &serde_rename_all);
                Some(quote! {
                    .serde_rename_all(#rule)
                })
            } else {
                None
            };

        let name = ident.to_string();
        tokens.extend(quote!{
            impl #ex_impl_generics #oapi::oapi::ToParameters<'__macro_gen_ex> for #ident #ty_generics #where_clause {
                fn to_parameters(components: &mut #oapi::oapi::Components) -> #oapi::oapi::Parameters {
                    #oapi::oapi::Parameters(#params.to_vec())
                }
            }
            impl #impl_generics #oapi::oapi::EndpointArgRegister for #ident #ty_generics #where_clause {
                fn register(components: &mut #oapi::oapi::Components, operation: &mut #oapi::oapi::Operation, _arg: &str) {
                    for parameter in <Self as #oapi::oapi::ToParameters>::to_parameters(components) {
                        operation.parameters.insert(parameter);
                    }
                }
            }
            impl #ex_impl_generics #salvo::Extractible<'__macro_gen_ex> for #ident #ty_generics #where_clause {
                fn metadata() -> &'__macro_gen_ex #salvo::extract::Metadata {
                    static METADATA: #salvo::__private::once_cell::sync::OnceCell<#salvo::extract::Metadata> = #salvo::__private::once_cell::sync::OnceCell::new();
                    METADATA.get_or_init(||
                        #salvo::extract::Metadata::new(#name)
                            .default_sources(vec![#default_source])
                            .fields(vec![#(#extract_fields),*])
                            #rename_all
                            #serde_rename_all
                    )
                }
                async fn extract(req: &'__macro_gen_ex mut #salvo::Request) -> Result<Self, impl #salvo::Writer + Send + std::fmt::Debug + 'static> {
                    #salvo::serde::from_request(req, Self::metadata()).await
                }
                async fn extract_with_arg(req: &'__macro_gen_ex mut #salvo::Request, _arg: &str) -> Result<Self, impl #salvo::Writer + Send + std::fmt::Debug + 'static> {
                    Self::extract(req).await
                }
            }
        });
    }
}

impl ToParameters {
    fn is_named_struct(&self) -> bool {
        matches!(&self.data, Data::Struct(data_struct) if matches!(&data_struct.fields, syn::Fields::Named(_)))
    }
    fn get_struct_fields(&self, field_names: &Option<&Vec<String>>) -> impl Iterator<Item = &Field> {
        let ident = &self.ident;
        let abort = |note: &str| {
            abort! {
                ident,
                "unsupported data type, expected struct with named fields `struct {} {{...}}` or unnamed fields `struct {}(...)`",
                ident.to_string(),
                ident.to_string();
                note = note
            }
        };

        match &self.data {
            Data::Struct(data_struct) => match &data_struct.fields {
                syn::Fields::Named(named_fields) => {
                    if field_names.is_some() {
                        abort! {ident, "`#[salvo(parameters(names(...)))]` is not supported attribute on a struct with named fields"}
                    }
                    named_fields.named.iter()
                }
                syn::Fields::Unnamed(unnamed_fields) => {
                    self.validate_unnamed_field_names(&unnamed_fields.unnamed, field_names);
                    unnamed_fields.unnamed.iter()
                }
                _ => abort("Unit type struct is not supported"),
            },
            _ => abort("Only struct type is supported"),
        }
    }

    fn validate_unnamed_field_names(
        &self,
        unnamed_fields: &Punctuated<Field, Token![,]>,
        field_names: &Option<&Vec<String>>,
    ) {
        let ident = &self.ident;
        match field_names {
            Some(names) => {
                if names.len() != unnamed_fields.len() {
                    abort! {
                        ident,
                        "declared names amount '{}' does not match to the unnamed fields amount '{}' in type: {}",
                            names.len(), unnamed_fields.len(), ident;
                        help = r#"Did you forget to add a field name to `#[salvo(parameters(names(... , "field_name")))]`"#;
                        help = "Or have you added extra name but haven't defined a type?"
                    }
                }
            }
            None => {
                abort! {
                    ident,
                    "struct with unnamed fields must have explicit name declarations.";
                    help = "Try defining `#[salvo(parameters(names(...)))]` over your type: {}", ident,
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct FieldParameterContainerAttributes<'a> {
    /// See [`ToParameterAttr::style`].
    default_style: &'a Option<Feature>,
    /// See [`ToParametersAttr::names`]. The name that applies to this field.
    name: Option<&'a String>,
    /// See [`ToParametersAttr::parameter_in`].
    default_parameter_in: &'a Option<Feature>,
    /// Custom rename all if serde attribute is not present.
    rename_all: Option<&'a RenameAll>,
}

struct FieldFeatures(Vec<Feature>);

impl_into_inner!(FieldFeatures);

impl Parse for FieldFeatures {
    fn parse(input: syn::parse::ParseStream) -> syn::Result<Self> {
        Ok(Self(parse_features!(
            // param features
            input as feature::ValueType,
            Rename,
            Style,
            feature::ParameterIn,
            AllowReserved,
            Example,
            Explode,
            SchemaWith,
            feature::Required,
            // param schema features
            Inline,
            Format,
            feature::Default,
            WriteOnly,
            ReadOnly,
            Nullable,
            XmlAttr,
            MultipleOf,
            Maximum,
            Minimum,
            ExclusiveMaximum,
            ExclusiveMinimum,
            MaxLength,
            MinLength,
            Pattern,
            MaxItems,
            MinItems,
            AdditionalProperties
        )))
    }
}

#[derive(Debug)]
struct Parameter<'a> {
    /// Field in the container used to create a single parameter.
    field: &'a Field,
    //// Field serde params parsed from field attributes.
    field_serde_params: Option<SerdeValue>,
    /// Attributes on the container which are relevant for this macro.
    container_attributes: FieldParameterContainerAttributes<'a>,
    /// Either serde rename all rule or to_parameters rename all rule if provided.
    serde_container: Option<&'a SerdeContainer>,
}

impl Parameter<'_> {
    /// Resolve [`Parameter`] features and split features into two [`Vec`]s. Features are split by
    /// whether they should be rendered in [`Parameter`] itself or in [`Parameter`]s schema.
    ///
    /// Method returns a tuple containing two [`Vec`]s of [`Feature`].
    fn resolve_field_features(&self) -> (Vec<Feature>, Vec<Feature>) {
        let field_features = self
            .field
            .attrs
            .iter()
            .filter_map(|attr| {
                if attr.path().is_ident("salvo") {
                    attribute::find_nested_list(attr, "parameter")
                        .ok()
                        .flatten()
                        .map(|metas| metas.parse_args::<FieldFeatures>().unwrap_or_abort().into_inner())
                } else {
                    None
                }
            })
            .reduce(|acc, item| acc.merge(item))
            .unwrap_or_default();

        field_features.into_iter().fold(
            (Vec::<Feature>::new(), Vec::<Feature>::new()),
            |(mut schema_features, mut param_features), feature| {
                match feature {
                    Feature::Inline(_)
                    | Feature::Format(_)
                    | Feature::Default(_)
                    | Feature::WriteOnly(_)
                    | Feature::ReadOnly(_)
                    | Feature::Nullable(_)
                    | Feature::XmlAttr(_)
                    | Feature::MultipleOf(_)
                    | Feature::Maximum(_)
                    | Feature::Minimum(_)
                    | Feature::ExclusiveMaximum(_)
                    | Feature::ExclusiveMinimum(_)
                    | Feature::MaxLength(_)
                    | Feature::MinLength(_)
                    | Feature::Pattern(_)
                    | Feature::MaxItems(_)
                    | Feature::MinItems(_)
                    | Feature::AdditionalProperties(_) => {
                        schema_features.push(feature);
                    }
                    _ => {
                        param_features.push(feature);
                    }
                };

                (schema_features, param_features)
            },
        )
    }

    fn to_extract_field_token_stream(&self, salvo: &Ident) -> TokenStream {
        let (_, mut param_features) = self.resolve_field_features();
        let name = self
            .field
            .ident
            .as_ref()
            .expect("struct field name should be exists")
            .to_string();

        let rename = param_features.pop_rename_feature().map(|rename| rename.into_value());
        let rename = rename.map(|rename| quote!(.rename(#rename)));
        let serde_rename = self.field_serde_params.as_ref().map(|field_param_serde| {
            field_param_serde
                .rename
                .as_ref()
                .map(|rename| quote!(.serde_rename(#rename)))
        });
        if let Some(parameter_in) = param_features.pop_parameter_in_feature() {
            let source = match parameter_in {
                feature::ParameterIn(crate::parameter::ParameterIn::Query) => {
                    quote! { #salvo::extract::metadata::Source::new(#salvo::extract::metadata::SourceFrom::Query, #salvo::extract::metadata::SourceParser::Smart) }
                }
                feature::ParameterIn(crate::parameter::ParameterIn::Header) => {
                    quote! { #salvo::extract::metadata::Source::new(#salvo::extract::metadata::SourceFrom::Header, #salvo::extract::metadata::SourceParser::Smart) }
                }
                feature::ParameterIn(crate::parameter::ParameterIn::Path) => {
                    quote! { #salvo::extract::metadata::Source::new(#salvo::extract::metadata::SourceFrom::Param, #salvo::extract::metadata::SourceParser::Smart) }
                }
                feature::ParameterIn(crate::parameter::ParameterIn::Cookie) => {
                    quote! { #salvo::extract::metadata::Source::new(#salvo::extract::metadata::SourceFrom::Cookie, #salvo::extract::metadata::SourceParser::Smart) }
                }
            };
            quote! {
                #salvo::extract::metadata::Field::new(#name)
                    .add_source(#source)
                    #rename
                    #serde_rename
            }
        } else {
            quote! {
                #salvo::extract::metadata::Field::new(#name)
                #rename
                #serde_rename
            }
        }
    }
}

impl ToTokens for Parameter<'_> {
    fn to_tokens(&self, tokens: &mut TokenStream) {
        let oapi = crate::oapi_crate();
        let field = self.field;
        let ident = &field.ident;
        let mut name = &*ident
            .as_ref()
            .map(|ident| ident.to_string())
            .or_else(|| self.container_attributes.name.cloned())
            .unwrap_or_else(|| {
                abort!(
                    field, "No name specified for unnamed field.";
                    help = "Try adding #[salvo(parameters(names(...)))] container attribute to specify the name for this field"
                )
            });

        if name.starts_with("r#") {
            name = &name[2..];
        }

        let (schema_features, mut param_features) = self.resolve_field_features();

        let rename = param_features
            .pop_rename_feature()
            .map(|rename| Cow::Owned(rename.into_value()))
            .or_else(|| {
                self.field_serde_params
                    .as_ref()
                    .and_then(|field_param_serde| field_param_serde.rename.as_deref().map(Cow::Borrowed))
            });
        let rename_all = self
            .container_attributes
            .rename_all
            .map(|rename_all| rename_all.as_rename_rule())
            .or_else(|| {
                self.serde_container
                    .as_ref()
                    .and_then(|serde_container| serde_container.rename_all.as_ref())
            });
        let name = crate::rename::<FieldRename>(name, rename, rename_all).unwrap_or(Cow::Borrowed(name));
        let type_tree = TypeTree::from_type(&field.ty);

        tokens.extend(quote! { #oapi::oapi::parameter::Parameter::new(#name)});

        if let Some(parameter_in) = param_features.pop_parameter_in_feature() {
            tokens.extend(quote! { .parameter_in(#parameter_in) });
        } else if let Some(parameter_in) = &self.container_attributes.default_parameter_in {
            tokens.extend(parameter_in.to_token_stream());
        }

        if let Some(style) = param_features.pop_style_feature() {
            tokens.extend(quote! { .style(#style) });
        } else if let Some(style) = &self.container_attributes.default_style {
            tokens.extend(style.to_token_stream());
        }

        if let Some(deprecated) = crate::get_deprecated(&field.attrs) {
            tokens.extend(quote! { .deprecated(#deprecated) });
        }

        let schema_with = pop_feature!(param_features => Feature::SchemaWith(_));
        if let Some(schema_with) = schema_with {
            tokens.extend(quote! { .schema(#schema_with) });
        } else {
            let description = CommentAttributes::from_attributes(&field.attrs).as_formatted_string();
            if !description.is_empty() {
                tokens.extend(quote! { .description(#description)})
            }

            let value_type = param_features.pop_value_type_feature();
            let component = value_type
                .as_ref()
                .map(|value_type| value_type.as_type_tree())
                .unwrap_or(type_tree);

            let required = pop_feature_as_inner!(param_features => Feature::Required(_v))
                .as_ref()
                .map(crate::feature::Required::is_true)
                .unwrap_or(false);

            let non_required = (component.is_option() && !required)
                || !crate::is_required(self.field_serde_params.as_ref(), self.serde_container);
            let required: Required = (!non_required).into();

            tokens.extend(quote! {
                .required(#required)
            });
            tokens.extend(param_features.to_token_stream());

            let schema = ComponentSchema::new(component::ComponentSchemaProps {
                type_tree: &component,
                features: Some(schema_features),
                description: None,
                deprecated: None,
                object_name: "",
                type_definition: false,
            });

            tokens.extend(quote! { .schema(#schema) });
        }
    }
}
