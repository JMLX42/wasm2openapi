use std::fs;
use std::path::PathBuf;

use actix_web::{App, HttpServer};
use clap::{Parser, Subcommand};
use utoipa::openapi::path::{
    OperationBuilder, Parameter, ParameterBuilder, ParameterIn, PathItemBuilder,
};
use utoipa::openapi::{
    ContentBuilder, InfoBuilder, ObjectBuilder, OpenApi, PathsBuilder, RefOr, ResponseBuilder,
    Schema,
};
use utoipa::PartialSchema;
use utoipa_swagger_ui::SwaggerUi;
use wit_component::DecodedWasm;
use wit_parser::{Function, WorldItem};

#[derive(Parser, Debug)]
#[clap(author, version, about, long_about = None)]
struct Cli {
    /// Path to the WebAssembly module file
    #[clap(short, long)]
    file: PathBuf,

    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Convert the WebAssembly module
    Convert,

    /// Serve the WebAssembly module
    Serve {
        /// Enable swagger documentation
        #[clap(long)]
        swagger: bool,
    },
}

fn parse_function_docs(docs: &str) -> (&str, Option<String>) {
    let mut lines = docs.lines();
    let summary = lines.next().unwrap_or_default();

    let description = lines
        .skip_while(|line| line.trim().is_empty()) // Skip any empty lines after the summary
        .collect::<Vec<&str>>()
        .join("\n");

    let description = if description.is_empty() {
        None
    } else {
        Some(description)
    };

    (summary, description)
}

struct Type(wit_parser::Type);

impl Type {
    fn into_schema(&self) -> RefOr<Schema> {
        match self.0 {
            wit_parser::Type::Bool => bool::schema().into(),
            wit_parser::Type::U8 => u8::schema().into(),
            wit_parser::Type::U16 => u16::schema().into(),
            wit_parser::Type::U32 => u32::schema().into(),
            wit_parser::Type::U64 => u64::schema().into(),
            wit_parser::Type::S8 => i8::schema().into(),
            wit_parser::Type::S16 => i16::schema().into(),
            wit_parser::Type::S32 => i32::schema().into(),
            wit_parser::Type::S64 => i64::schema().into(),
            wit_parser::Type::Float32 => f32::schema().into(),
            wit_parser::Type::Float64 => f64::schema().into(),
            wit_parser::Type::Char => char::schema().into(),
            wit_parser::Type::String => String::schema().into(),
            wit_parser::Type::Id(_) => String::schema().into(),
        }
    }
}

struct Results(wit_parser::Results);

impl Results {
    fn into_schema(&self) -> RefOr<Schema> {
        match &self.0 {
            wit_parser::Results::Named(params) => RefOr::T(Schema::Object(
                params
                    .iter()
                    .fold(ObjectBuilder::new(), |obj, (name, ty)| {
                        obj.property(name, Type(ty.clone()).into_schema())
                    })
                    .build(),
            )),
            wit_parser::Results::Anon(ty) => Type(*ty).into_schema(),
        }
    }
}

fn get_function_parameters(function: &Function) -> Vec<Parameter> {
    function
        .params
        .iter()
        .enumerate()
        .map(|(_index, (name, ty))| {
            ParameterBuilder::new()
                .name(name.clone())
                .required(utoipa::openapi::Required::True)
                .parameter_in(ParameterIn::Query)
                .schema(Some(Type(ty.clone()).into_schema()))
                .build()
        })
        .collect()
}

fn get_function_operation(function: &Function) -> OperationBuilder {
    let docs = function.docs.contents.clone().unwrap_or_default();
    let (summary, description) = parse_function_docs(docs.as_str());
    let parameters = get_function_parameters(function);

    OperationBuilder::new()
        .operation_id(Some(function.name.clone()))
        .summary(Some(summary))
        .description(description)
        .response(
            "200",
            ResponseBuilder::new()
                .content(
                    "application/json",
                    ContentBuilder::new()
                        .schema(Results(function.results.clone()).into_schema())
                        .build(),
                )
                .build(),
        )
        .parameters(Some(parameters))
}

fn get_openapi_paths(functions: Vec<(&String, &Function)>) -> PathsBuilder {
    functions
        .into_iter()
        .filter_map(|(world_name, function)| {
            let path = format!("/{}/{}", world_name, function.name);
            let operation = get_function_operation(function);

            Some((
                path,
                PathItemBuilder::new()
                    .operation(utoipa::openapi::PathItemType::Post, operation)
                    .build(),
            ))
        })
        .fold(PathsBuilder::new(), |paths, (p, i)| paths.path(p, i))
}

fn load_wasm_component_functions(wit: &DecodedWasm) -> Vec<(&String, &Function)> {
    // Find the exported functions
    let functions = wit.resolve().worlds.iter().flat_map(|(_id, world)| {
        world.exports.iter().filter_map(|(_, item)| match item {
            // ! For some reason world.name is always "root".
            // ! https://github.com/bytecodealliance/wasm-tools/issues/1315
            WorldItem::Function(func) => Some((&world.name, func)),
            _ => None,
        })
    });

    functions.collect()
}

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let args = Cli::parse();
    // Load the WASM module
    let module = fs::read(args.file).expect("Failed to load module");
    let wit = wit_component::decode(&module).expect("Failed to decode WIT component");
    let functions = load_wasm_component_functions(&wit);
    let paths = get_openapi_paths(functions);
    let openapi = OpenApi::new(
        // TODO: call a special openapi_info() component function
        InfoBuilder::new()
            .title("WASM Component API")
            .version("1.0")
            .description(Some("OpenAPI definition of a WASM component."))
            .build(),
        paths,
    );

    match args.command {
        Command::Convert => println!("{}", serde_json::to_string(&openapi).unwrap()),
        Command::Serve { swagger } => {
            HttpServer::new(move || {
                let app = App::new();

                // TODO: actually serve the API endpoints and call the WASM component.

                if swagger {
                    app.service(
                        SwaggerUi::new("/swagger-ui/{_:.*}")
                            .url("/api-docs/openapi.json", openapi.clone()),
                    )
                } else {
                    app
                }
            })
            .bind(("127.0.0.1", 8080))?
            .run()
            .await?;
        }
    };

    Ok(())
}
