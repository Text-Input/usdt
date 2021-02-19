//! Prototype proc-macro crate for parsing a DTrace provider definition into Rust code.
// Copyright 2021 Oxide Computer Company

use std::fs::read_to_string;

use pest::Parser;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Lit};

mod dtrace;
use dtrace::{DTraceParser, Rule};

/// Parse a DTrace provider file into a Rust struct.
///
/// This macro parses a DTrace provider.d file, given as a single literal string path. It then
/// generates a Rust struct definition and implementation, with associated functions in the impl
/// for each of the DTrace probe definitions. This is a simple way of generating Rust functions
/// that can be called normally, but which are intended to actually be DTrace probe points.
///
/// For example, assume the file `"foo.d"` has the following contents:
///
/// ```ignore
/// provider foo {
///     probe bar();
///     probe base(uint8_t, string);
/// };
/// ```
///
/// In a Rust library or application, write:
///
/// ```ignore
/// dtrace_provider!("foo.d");
/// ```
///
/// One can run `cargo expand` to see the generated code, the relevant section of which should
/// look like this:
///
/// ```no_run
/// #[allow(non_camel_case_types)]
/// #[allow(dead_code)]
/// pub struct foo;
///
/// impl foo {
///     #[allow(dead_code)]
///     pub fn bar() { }
///     
///     #[allow(dead_code)]
///     pub fn baz(arg0: u8, arg1: String) {}
/// }
/// ```
///
/// One can then instrument the application or library as one might expect:
///
/// ```ignore
/// fn do_stuff(count: u8, name: String) {
///     // doing stuff
///     foo::baz(count, name.clone());
/// }
/// ```
///
/// Note
/// ----
/// This macro currently supports only a subset of the full D language, with the focus being on
/// parsing a provider definition. As such, pragmas, predicates, and actions are not supported. The
/// only supported probe argument types are integers of specific bit-width, e.g., `uint16_t`,
/// `string`s, `float`s, and `double`s.
#[proc_macro]
pub fn dtrace_provider(item: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let tok = parse_macro_input!(item as Lit);
    let filename = match tok {
        Lit::Str(f) => f.value(),
        _ => panic!("DTrace provider must be a single literal string filename"),
    };
    let contents = read_to_string(filename).expect("Could not read provider definition file");
    let mut contents = DTraceParser::parse(Rule::PROVIDER, &contents)
        .expect("DTrace provider file contents are not valid");
    let mut provider = contents
        .next()
        .expect("Expected at least one token parsing DTrace provider")
        .into_inner();

    // First token is the literal "provider", second is the name
    let _ = provider.next();
    let provider_name = provider.next().expect("Expected a provider name").as_str();
    let provider_ident = format_ident!("{}", provider_name);

    let mut probes = Vec::new();
    for pair in provider {
        if matches!(pair.as_rule(), Rule::PROBE) {
            let mut pairs = pair.into_inner();

            // First token is the literal "probe", next is the name
            let probe = pairs.next().expect("Expected the literal \"probe\"");
            assert!(
                matches!(probe.as_rule(), Rule::PROBE_KEY),
                "Expected the literal \"probe\""
            );
            let probe_name = pairs.next().expect("Expected a probe name").as_str();
            let probe_ident = format_ident!("{}", probe_name);
            let mut argument_list = pairs
                .next()
                .expect("Expected an argument list")
                .into_inner();

            // Parse the list of probe argument data types, generating a function signature
            let left_paren = argument_list.next().expect("Expected a literal \"(\")");
            assert!(
                matches!(left_paren.as_rule(), Rule::LEFT_PAREN),
                "Expected a literal \"(\" to open the probe argument list"
            );
            let possibly_argument = argument_list.next().expect("Expected an argument list");
            let mut probe_arguments = Vec::new();
            let mut probe_inputs = Vec::new();
            if matches!(possibly_argument.as_rule(), Rule::ARGUMENT) {
                let data_types = possibly_argument.into_inner();
                // The point of this loop is to generate an actual argument signature from each
                // DTrace argument token. For example, this makes the following transformation:
                //
                // (uint8_t, string) -> (arg0: u8, arg1: String)
                for (i, data_type) in data_types.enumerate() {
                    let inner = data_type.into_inner();
                    for pair in inner {
                        let arg = format_ident!("arg{}", i);
                        let typ = match pair.as_rule() {
                            Rule::UNSIGNED_INT => {
                                let bit_width: u8 = pair.into_inner().as_str().parse().unwrap();
                                format_ident!("u{}", bit_width)
                            }
                            Rule::SIGNED_INT => {
                                let bit_width: u8 = pair.into_inner().as_str().parse().unwrap();
                                format_ident!("i{}", bit_width)
                            }
                            Rule::STRING => format_ident!("String"),
                            Rule::FLOAT => format_ident!("f32"),
                            Rule::DOUBLE => format_ident!("f64"),
                            _ => {
                                unreachable!(format!(
                                    "Parsed unexpected DTrace argument type: {}",
                                    pair
                                ));
                            }
                        };
                        probe_arguments.push(quote! {#arg: #typ});
                        probe_inputs.push(quote! {#arg});
                    }
                }
            } else {
                assert!(
                    matches!(possibly_argument.as_rule(), Rule::RIGHT_PAREN),
                    "Expected a literal \")\" to close an empty probe argument list"
                );
            }

            let print_args = &probe_inputs.iter().map(|_| " {}").collect::<String>();
            let print_fmt = format!("{}{}", "probe {}:{}", print_args);

            // Construct the full function signature for the corresponding DTrace probe. The list
            // of these will be expanded inside the resulting impl block below.
            probes.push(quote! {
                #[allow(dead_code)]
                pub fn #probe_ident(#(#probe_arguments,)*) {
                    println!(stringify!(#print_fmt), #provider_name, #probe_name, #(#probe_inputs,)*);
                }
            });
        }
    }

    // Build the actual probe definition.
    //
    // This is a simple public unit struct and an impl block with a public function for each probe,
    // with signatures matching the provider definition.
    (quote! {
        #[allow(non_camel_case_types)]
        #[allow(dead_code)]
        pub struct #provider_ident;

        impl #provider_ident {
            #(#probes)*
        }
    })
    .into()
}
