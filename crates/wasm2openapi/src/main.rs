use std::collections::HashMap;
use std::fs;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use actix_web::http::header::ContentType;
use actix_web::{web, App, HttpResponse, HttpServer, Responder};
use clap::{Parser, Subcommand};
use serde_json::Number;
use utoipa::openapi::path::{Operation, OperationBuilder, PathItemBuilder};
use utoipa::openapi::request_body::{RequestBody, RequestBodyBuilder};
use utoipa::openapi::{
    ContentBuilder, InfoBuilder, ObjectBuilder, OpenApiBuilder, PathItem, PathItemType,
    PathsBuilder, RefOr, ResponseBuilder, Schema, ServerBuilder,
};
use utoipa::PartialSchema;
use utoipa_swagger_ui::SwaggerUi;
use wasmtime::component::{Component, Instance, Linker, Val};
use wasmtime::{AsContextMut, Config, Engine, Store};
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
        #[clap(long, short)]
        swagger: bool,

        /// Specify the server's bind address
        #[clap(long, short, default_value = "127.0.0.1")]
        address: String,

        /// Specify the server's bind port
        #[clap(long, short, default_value_t = 8080)]
        port: u16,
    },
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

#[derive(Debug, Clone)]
struct Endpoint {
    pub path: String,
    pub prototype: wit_parser::Function,
    pub callable: wasmtime::component::Func,
}

impl Endpoint {
    pub fn new(
        path: String,
        prototype: wit_parser::Function,
        callable: wasmtime::component::Func,
    ) -> Self {
        Self {
            path,
            prototype,
            callable,
        }
    }
}

struct Value(Val);

impl Deref for Value {
    type Target = Val;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Value {
    pub fn from_json(v: &serde_json::Value, ty: &wit_parser::Type) -> Self {
        Self(match ty {
            wit_parser::Type::Bool => Val::Bool(v.as_bool().unwrap()),
            wit_parser::Type::U8 => Val::U8(v.as_u64().unwrap() as u8),
            wit_parser::Type::U16 => Val::U16(v.as_u64().unwrap() as u16),
            wit_parser::Type::U32 => Val::U32(v.as_u64().unwrap() as u32),
            wit_parser::Type::U64 => Val::U64(v.as_u64().unwrap()),
            wit_parser::Type::S8 => Val::S8(v.as_i64().unwrap() as i8),
            wit_parser::Type::S16 => Val::S16(v.as_i64().unwrap() as i16),
            wit_parser::Type::S32 => Val::S32(v.as_i64().unwrap() as i32),
            wit_parser::Type::S64 => Val::S64(v.as_i64().unwrap()),
            wit_parser::Type::Float32 => Val::Float32(v.as_f64().unwrap() as f32),
            wit_parser::Type::Float64 => Val::Float64(v.as_f64().unwrap()),
            wit_parser::Type::String => Val::String(v.as_str().unwrap().to_string().into()),
            wit_parser::Type::Char => Val::Char(v.as_str().unwrap().as_bytes()[0] as char),
            // TODO
            wit_parser::Type::Id(_) => todo!(),
        })
    }

    pub fn to_json(&self) -> serde_json::Value {
        match &self.0 {
            Val::Bool(v) => serde_json::Value::Bool(*v),
            Val::S8(v) => serde_json::Value::Number(Number::from(*v)),
            Val::U8(v) => serde_json::Value::Number(Number::from(*v)),
            Val::S16(v) => serde_json::Value::Number(Number::from(*v)),
            Val::U16(v) => serde_json::Value::Number(Number::from(*v)),
            Val::S32(v) => serde_json::Value::Number(Number::from(*v)),
            Val::U32(v) => serde_json::Value::Number(Number::from(*v)),
            Val::S64(v) => serde_json::Value::Number(Number::from(*v)),
            Val::U64(v) => serde_json::Value::Number(Number::from(*v)),
            Val::Float32(v) => serde_json::Value::Number(Number::from_f64(*v as f64).unwrap()),
            Val::Float64(v) => serde_json::Value::Number(Number::from_f64(*v).unwrap()),
            Val::Char(v) => serde_json::Value::String(v.clone().to_string()),
            Val::String(v) => serde_json::Value::String(v.clone().into_string()),
            // TODO
            Val::List(_) => todo!(),
            Val::Record(_) => todo!(),
            Val::Tuple(_) => todo!(),
            Val::Variant(_) => todo!(),
            Val::Enum(_) => todo!(),
            Val::Option(_) => todo!(),
            Val::Result(_) => todo!(),
            Val::Flags(_) => todo!(),
            Val::Resource(_) => todo!(),
        }
    }
}

impl Endpoint {
    pub fn call(
        &self,
        state: web::Data<Arc<Mutex<Store<()>>>>,
        payload: web::Json<HashMap<String, serde_json::Value>>,
    ) -> impl Responder {
        let mut store = state.lock().unwrap();
        // TODO: handle errors as a 400 + error response
        let parameters = self.decode_parameters(payload).unwrap();
        let mut results = vec![Val::Bool(false); self.prototype.results.len()];

        let res = self
            .callable
            .call(store.as_context_mut(), &parameters, &mut results);
        // TODO: 500 error
        self.callable.post_return(store.as_context_mut()).unwrap();

        match res {
            Ok(_) => HttpResponse::Ok()
                .content_type(ContentType::json())
                .json(Value(results[0].clone()).to_json()),
            Err(_) => HttpResponse::BadRequest()
                .content_type(ContentType::json())
                .body("{}"),
        }
    }

    fn decode_parameters(
        &self,
        payload: web::Json<HashMap<String, serde_json::Value>>,
    ) -> Result<Vec<Val>, ()> {
        let params = self
            .prototype
            .params
            .iter()
            .map(|(name, ty)| {
                // TODO: handle missing param error (400 + error message)
                let v = payload.get(name).unwrap();

                // TODO: handle type mismatch error (400 + error message)
                Value::from_json(v, ty).0
            })
            .fold(vec![], |mut params, v| {
                params.push(v);
                params
            });

        Ok(params)
    }

    fn function_request_body(&self) -> RequestBody {
        // TODO: Add support for JSON-RPC
        RequestBodyBuilder::new()
            .content(
                ContentType::json().to_string(),
                ContentBuilder::new()
                    .schema(
                        self.prototype
                            .params
                            .iter()
                            .fold(ObjectBuilder::new(), |obj, (name, ty)| {
                                obj.property(name, Type(ty.clone()).into_schema())
                            })
                            .build(),
                    )
                    .build(),
            )
            .build()
    }

    fn parse_function_docs(&self) -> (String, Option<String>) {
        let docs = self.prototype.docs.contents.clone().unwrap_or_default();
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

        (summary.into(), description)
    }

    fn result_schema(&self) -> RefOr<Schema> {
        match &self.prototype.results {
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

impl Into<Operation> for Endpoint {
    fn into(self) -> Operation {
        let (summary, description) = self.parse_function_docs();
        let body = self.function_request_body();

        OperationBuilder::new()
            .operation_id(Some(self.prototype.name.clone()))
            .summary(Some(summary))
            .description(description)
            .request_body(Some(body))
            .response(
                "200",
                ResponseBuilder::new()
                    .content(
                        ContentType::json().to_string(),
                        ContentBuilder::new().schema(self.result_schema()).build(),
                    )
                    .build(),
            )
            .build()
    }
}

impl Into<PathItem> for Endpoint {
    fn into(self) -> PathItem {
        let operation: Operation = self.into();

        PathItemBuilder::new()
            .operation(PathItemType::Post, operation)
            .build()
    }
}

fn list_wasm_component_functions(wit: &DecodedWasm) -> Vec<(&String, &Function)> {
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

fn get_endpoints<T>(
    functions: Vec<(&String, &Function)>,
    mut context: impl AsContextMut<Data = T>,
    component_instance: &Instance,
) -> Vec<Endpoint> {
    let mut endpoints = vec![];

    for (world_name, function) in functions {
        endpoints.push(Endpoint::new(
            format!("/{}/{}", world_name, function.name),
            function.clone(),
            component_instance
                .get_func(context.as_context_mut(), &function.name)
                .unwrap(),
        ))
    }

    endpoints
}

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    pretty_env_logger::init();

    let args = Cli::parse();

    // Load the WASM component
    let data = fs::read(args.file).expect("Failed to read module");

    // Instantiate the WASM component
    let config = {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config
    };
    let engine = Engine::new(&config).expect("Failed to create WASM engine");
    let component = Component::from_binary(&engine, &data).expect("Failed to load component");
    let linker: Linker<()> = Linker::new(&engine);
    let store = Arc::new(Mutex::new(Store::new(&engine, ())));
    let instance = linker
        .instantiate(store.lock().unwrap().as_context_mut(), &component)
        .expect("Failed to instantiate component");

    // Decode the component's WIT
    let wit = wit_component::decode(&data).expect("Failed to decode WIT component");
    let functions = list_wasm_component_functions(&wit);

    let endpoints = get_endpoints(functions, store.lock().unwrap().as_context_mut(), &instance);

    // Build the OpenAPI declaration
    let paths = endpoints
        .clone()
        .into_iter()
        .fold(PathsBuilder::new(), |paths, e| {
            paths.path(e.path.clone(), e.into())
        });
    let openapi = OpenApiBuilder::new()
        // TODO: call a special openapi_info() component function
        .info(
            InfoBuilder::new()
                .title("WASM Component API")
                .version("1.0")
                .description(Some("OpenAPI definition of a WASM component."))
                .build(),
        )
        .paths(paths);

    match args.command {
        Command::Convert => {
            println!("{}", serde_json::to_string(&openapi.build()).unwrap())
        }
        Command::Serve {
            swagger,
            address,
            port,
        } => {
            let openapi = openapi
                .servers(Some(vec![ServerBuilder::new()
                    .url(format!("http://{}:{}", address, port))
                    .build()]))
                .build();

            HttpServer::new(move || {
                let app = App::new().app_data(web::Data::new(store.clone()));
                let app = if swagger {
                    app.service(
                        SwaggerUi::new("/swagger-ui/{_:.*}")
                            .url("/api-docs/openapi.json", openapi.clone()),
                    )
                } else {
                    app
                };

                endpoints.clone().into_iter().fold(app, |app, endpoint| {
                    app.route(
                        &endpoint.clone().path,
                        web::post().to(move |state, payload| {
                            let endpoint = endpoint.clone();

                            async move { endpoint.call(state, payload) }
                        }),
                    )
                })
            })
            .bind((address, port))?
            .run()
            .await?;
        }
    };

    Ok(())
}
