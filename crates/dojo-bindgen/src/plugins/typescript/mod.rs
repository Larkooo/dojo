use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use cainome::parser::tokens::{Composite, CompositeType, Function, Token};
use dojo_world::manifest::utils::{capitalize, get_name_from_tag};

use crate::error::BindgenResult;
use crate::plugins::BuiltinPlugin;
use crate::{DojoContract, DojoData, DojoModel};

pub struct TypescriptPlugin {}

impl TypescriptPlugin {
    pub fn new() -> Self {
        Self {}
    }

    // Maps cairo types to C#/Unity SDK defined types
    fn map_type(token: &Token, generic_args: &Vec<(String, Token)>) -> String {
        match token.type_name().as_str() {
            "bool" => "RecsType.Boolean".to_string(),
            "u8" => "RecsType.Number".to_string(),
            "u16" => "RecsType.Number".to_string(),
            "u32" => "RecsType.Number".to_string(),
            "u64" => "RecsType.Number".to_string(),
            "u128" => "RecsType.BigInt".to_string(),
            "u256" => "RecsType.BigInt".to_string(),
            "usize" => "RecsType.Number".to_string(),
            "felt252" => "RecsType.BigInt".to_string(),
            "bytes31" => "RecsType.String".to_string(),
            "ClassHash" => "RecsType.BigInt".to_string(),
            "ContractAddress" => "RecsType.BigInt".to_string(),
            "ByteArray" => "RecsType.String".to_string(),
            "array" => {
                if let Token::Array(array) = token {
                    format!("{}[]", TypescriptPlugin::map_type(&array.inner, generic_args))
                } else {
                    panic!("Invalid array token: {:?}", token);
                }
            }
            "tuple" => {
                if let Token::Tuple(tuple) = token {
                    let inners = tuple
                        .inners
                        .iter()
                        .map(|inner| TypescriptPlugin::map_type(inner, generic_args))
                        .collect::<Vec<String>>()
                        .join(", ");

                    format!("[{}]", inners)
                } else {
                    panic!("Invalid tuple token: {:?}", token);
                }
            }
            "generic_arg" => {
                if let Token::GenericArg(arg) = &token {
                    let arg_type = generic_args
                        .iter()
                        .find(|(name, _)| name == arg)
                        .unwrap_or_else(|| panic!("Generic arg not found: {}", arg))
                        .1
                        .clone();

                    TypescriptPlugin::map_type(&arg_type, generic_args)
                } else {
                    panic!("Invalid generic arg token: {:?}", token);
                }
            }

            _ => token.type_name().to_string(),
        }
    }

    fn generated_header() -> String {
        format!(
            "// Generated by dojo-bindgen on {}. Do not modify this file manually.\n",
            chrono::Utc::now().to_rfc2822()
        )
    }

    // Token should be a struct
    // This will be formatted into a C# struct
    // using C# and unity SDK types
    fn format_struct(token: &Composite, handled_tokens: &[Composite]) -> String {
        let mut native_fields = String::new();
        let mut fields = String::new();

        for field in &token.inners {
            let mapped = TypescriptPlugin::map_type(&field.token, &token.generic_args);
            if mapped == field.token.type_name() {
                let token = handled_tokens
                    .iter()
                    .find(|t| t.type_name() == field.token.type_name())
                    .unwrap_or_else(|| panic!("Token not found: {}", field.token.type_name()));
                if token.r#type == CompositeType::Enum {
                    native_fields += format!("{}: {};\n    ", field.name, mapped).as_str();
                    fields += format!("{}: RecsType.Number,\n    ", field.name).as_str();
                } else {
                    native_fields +=
                        format!("{}: {};\n    ", field.name, field.token.type_name()).as_str();
                    fields += format!("{}: {}Definition,\n    ", field.name, mapped).as_str();
                }
            } else {
                native_fields +=
                    format!("{}: {};\n    ", field.name, mapped.replace("RecsType.", "")).as_str();
                fields += format!("{}: {},\n    ", field.name, mapped).as_str();
            }
        }

        format!(
            "
// Type definition for `{path}` struct
export interface {name} {{
    {native_fields}
}}

export const {name}Definition = {{
    {fields}
}};
",
            path = token.type_path,
            name = token.type_name(),
            fields = fields,
            native_fields = native_fields
        )
    }

    // Token should be an enum
    // This will be formatted into a C# enum
    // Enum is mapped using index of cairo enum
    fn format_enum(token: &Composite) -> String {
        let name = token.type_name();

        let mut result = format!(
            "
// Type definition for `{}` enum
type {} = ",
            token.type_path, name
        );

        let mut variants = Vec::new();

        for field in &token.inners {
            let field_type =
                TypescriptPlugin::map_type(&field.token, &token.generic_args).replace("()", "");

            let variant_definition = if field_type.is_empty() {
                // No associated data
                format!("{{ type: '{}'; }}", field.name)
            } else {
                // With associated data
                format!("{{ type: '{}'; data: {}; }}", field.name, field_type)
            };

            variants.push(variant_definition);
        }

        result += &variants.join(" | ");

        result
    }

    // Token should be a model
    // This will be formatted into a C# class inheriting from ModelInstance
    // Fields are mapped using C# and unity SDK types
    fn format_model(model: &Composite, handled_tokens: &[Composite]) -> String {
        let mut custom_types = Vec::<String>::new();
        let mut types = Vec::<String>::new();
        let fields = model
            .inners
            .iter()
            .map(|field| {
                let mapped = TypescriptPlugin::map_type(&field.token, &model.generic_args);
                if mapped == field.token.type_name() {
                    custom_types.push(format!("\"{}\"", field.token.type_name()));

                    let token = handled_tokens
                        .iter()
                        .find(|t| t.type_name() == field.token.type_name())
                        .unwrap_or_else(|| panic!("Token not found: {}", field.token.type_name()));
                    if token.r#type == CompositeType::Enum {
                        format!("{}: {},", field.name, "RecsType.Number")
                    } else {
                        format!("{}: {}Definition,", field.name, mapped)
                    }
                } else {
                    types.push(format!("\"{}\"", field.token.type_name()));
                    format!("{}: {},", field.name, mapped)
                }
            })
            .collect::<Vec<String>>()
            .join("\n                    ");

        format!(
            "
        // Model definition for `{path}` model
        {model}: (() => {{
            return defineComponent(
                world,
                {{
                    {fields}
                }},
                {{
                    metadata: {{
                        name: \"{model}\",
                        types: [{types}],
                        customTypes: [{custom_types}],
                    }},
                }}
            );
        }})(),
",
            path = model.type_path,
            model = model.type_name(),
            fields = fields,
            types = types.join(", "),
            custom_types = custom_types.join(", ")
        )
    }

    // Handles a model definition and its referenced tokens
    // Will map all structs and enums to TS types
    // Will format the models into a object
    fn handle_model(&self, models: &[&DojoModel], handled_tokens: &mut Vec<Composite>) -> String {
        let mut out = String::new();
        out += TypescriptPlugin::generated_header().as_str();
        out += "import { defineComponent, Type as RecsType, World } from \"@dojoengine/recs\";\n";
        out += "\n";
        out += "export type ContractComponents = Awaited<
            ReturnType<typeof defineContractComponents>
        >;\n";

        out += "\n\n";

        let mut models_structs = Vec::new();
        for model in models {
            let tokens = &model.tokens;

            for token in &tokens.enums {
                handled_tokens.push(token.to_composite().unwrap().to_owned());
            }
            for token in &tokens.structs {
                handled_tokens.push(token.to_composite().unwrap().to_owned());
            }

            let mut structs = tokens.structs.to_owned();
            structs.sort_by(|a, b| {
                if a.to_composite()
                    .unwrap()
                    .inners
                    .iter()
                    .any(|field| field.token.type_name() == b.type_name())
                {
                    std::cmp::Ordering::Greater
                } else {
                    std::cmp::Ordering::Less
                }
            });

            for token in &structs {
                if handled_tokens.iter().filter(|t| t.type_name() == token.type_name()).count() > 1
                {
                    continue;
                }

                // first index is our model struct
                if token.type_name() == get_name_from_tag(&model.tag) {
                    models_structs.push(token.to_composite().unwrap().clone());
                }

                out +=
                    TypescriptPlugin::format_struct(token.to_composite().unwrap(), handled_tokens)
                        .as_str();
            }

            for token in &tokens.enums {
                if handled_tokens.iter().filter(|t| t.type_name() == token.type_name()).count() > 1
                {
                    continue;
                }
                out += TypescriptPlugin::format_enum(token.to_composite().unwrap()).as_str();
            }

            out += "\n";
        }

        out += "
export function defineContractComponents(world: World) {
    return {
";

        for model in models_structs {
            out += TypescriptPlugin::format_model(&model, handled_tokens).as_str();
        }

        out += "    };
}\n";

        out
    }

    // Formats a system into a C# method used by the contract class
    // Handled tokens should be a list of all structs and enums used by the contract
    // Such as a set of referenced tokens from a model
    fn format_system(system: &Function, handled_tokens: &[Composite]) -> String {
        fn map_type(token: &Token) -> String {
            match token {
                Token::CoreBasic(_) => TypescriptPlugin::map_type(token, &vec![])
                .replace("RecsType.", "")
                // types should be lowercased
                .to_lowercase(),
                Token::Composite(t) => format!("models.{}", t.type_name()),
                Token::Array(t) => format!("{}[]", map_type(&t.inner)),
                _ => panic!("Unsupported token type: {:?}", token),
            }
        }

        let args = system
            .inputs
            .iter()
            .map(|arg| format!("{}: {}", arg.0, map_type(&arg.1)))
            .collect::<Vec<String>>()
            .join(", ");

        let calldata = system
            .inputs
            .iter()
            .map(|arg| {
                let token = &arg.1;
                let type_name = &arg.0;

                match handled_tokens.iter().find(|t| t.type_name() == token.type_name()) {
                    Some(t) => {
                        // Need to flatten the struct members.
                        match t.r#type {
                            CompositeType::Struct => t
                                .inners
                                .iter()
                                .map(|field| format!("props.{}.{}", type_name, field.name))
                                .collect::<Vec<String>>()
                                .join(",\n                    "),
                            _ => {
                                format!("props.{}", type_name)
                            }
                        }
                    }
                    None => format!("props.{}", type_name),
                }
            })
            .collect::<Vec<String>>()
            .join(",\n                ");

        format!(
            "
        // Call the `{system_name}` system with the specified Account and calldata
        const {system_name} = async (props: {{ account: Account{arg_sep}{args} }}) => {{
            try {{
                return await provider.execute(
                    props.account,
                    contract_name,
                    \"{system_name}\",
                    [{calldata}]
                );
            }} catch (error) {{
                console.error(\"Error executing spawn:\", error);
                throw error;
            }}
        }};
            ",
            // selector for execute
            system_name = system.name,
            // add comma if we have args
            arg_sep = if !args.is_empty() { ", " } else { "" },
            // formatted args to use our mapped types
            args = args,
            // calldata for execute
            calldata = calldata
        )
    }

    // Formats a contract tag into a pretty contract name
    // eg. dojo_examples:actions -> Actions
    fn formatted_contract_name(tag: &str) -> String {
        capitalize(&get_name_from_tag(tag))
    }

    // Handles a contract definition and its underlying systems
    // Will format the contract into a C# class and
    // all systems into C# methods
    // Handled tokens should be a list of all structs and enums used by the contract
    fn handle_contracts(
        &self,
        contracts: &[&DojoContract],
        handled_tokens: &[Composite],
    ) -> String {
        let mut out = String::new();
        out += TypescriptPlugin::generated_header().as_str();
        out += "import { Account } from \"starknet\";\n";
        out += "import { DojoProvider } from \"@dojoengine/core\";\n";
        out += "import * as models from \"./models.gen\";\n";
        out += "\n";
        out += "export type IWorld = Awaited<ReturnType<typeof setupWorld>>;";

        out += "\n\n";

        out += "export async function setupWorld(provider: DojoProvider) {";

        for contract in contracts {
            let systems = contract
                .systems
                .iter()
                .map(|system| {
                    TypescriptPlugin::format_system(system.to_function().unwrap(), handled_tokens)
                })
                .collect::<Vec<String>>()
                .join("\n\n    ");

            out += &format!(
                "
    // System definitions for `{}` contract
    function {}() {{
        const contract_name = \"{}\";

        {}

        return {{
            {}
        }};
    }}
",
                contract.tag,
                // capitalize contract name
                TypescriptPlugin::formatted_contract_name(&contract.tag),
                TypescriptPlugin::formatted_contract_name(&contract.tag),
                systems,
                contract
                    .systems
                    .iter()
                    .map(|system| { system.to_function().unwrap().name.to_string() })
                    .collect::<Vec<String>>()
                    .join(", ")
            );
        }

        out += "
    return {
        ";

        out += &contracts
            .iter()
            .map(|c| {
                format!(
                    "{}: {}()",
                    TypescriptPlugin::formatted_contract_name(&c.tag),
                    TypescriptPlugin::formatted_contract_name(&c.tag)
                )
            })
            .collect::<Vec<String>>()
            .join(",\n        ");

        out += "
    };
}\n";

        out
    }
}

#[async_trait]
impl BuiltinPlugin for TypescriptPlugin {
    async fn generate_code(&self, data: &DojoData) -> BindgenResult<HashMap<PathBuf, Vec<u8>>> {
        let mut out: HashMap<PathBuf, Vec<u8>> = HashMap::new();
        let mut handled_tokens = Vec::<Composite>::new();

        // Handle codegen for models
        let models_path = Path::new("models.gen.ts").to_owned();
        let models = data.models.values().collect::<Vec<_>>();
        let code = self.handle_model(models.as_slice(), &mut handled_tokens);

        out.insert(models_path, code.as_bytes().to_vec());

        // Handle codegen for contracts & systems
        let contracts_path = Path::new("contracts.gen.ts").to_owned();
        let contracts = data.contracts.values().collect::<Vec<_>>();
        let code = self.handle_contracts(contracts.as_slice(), &handled_tokens);

        out.insert(contracts_path, code.as_bytes().to_vec());

        Ok(out)
    }
}
