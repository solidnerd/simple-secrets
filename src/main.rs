extern crate iron;
extern crate router;
extern crate etcd;
extern crate futures;
extern crate tokio_core;
extern crate hyper;
extern crate argonautica;
extern crate rand;
extern crate uuid;
extern crate fruently;

#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate error_chain;

use iron::prelude::*;
use iron::headers::*;
use router::Router;
use etcd::kv::{self};
use futures::Future;
use tokio_core::reactor::Core;
use fruently::fluent::Fluent;
use fruently::forwardable::JsonForwardable;

use rand::{Rng, thread_rng};
use rand::distributions::Alphanumeric;

use errors::*;

lazy_static! {
    // Program options
    static ref ETCD_CLUSTER_MEMBERS: &'static str = {
        if let Ok(val) = std::env::var("ETCD_CLUSTER_MEMBERS") {
            Box::leak(val.into_boxed_str())
        } else {
            "http://localhost:2379"
        }
    };
    static ref TOKEN_EXPIRATION_SECS: u64 = {
        if let Ok(val) = std::env::var("TOKEN_EXPIRATION_SECS") {
            str::parse::<u64>(&val).unwrap_or(600)
        } else {
            600
        }
    };

    // Application instance SPIFFE ID
    static ref SPIFFE_ID: &'static str = "spiffe://example.org/simple-secrets";

    // Fluentd client
    static ref FLUENTD_FORWARD_ADDR: &'static str = {
        if let Ok(val) = std::env::var("FLUENTD_FORWARD_ADDR") {
            Box::leak(val.into_boxed_str())
        } else {
            "127.0.0.1:24224"
        }
    };
    static ref fluentd_client: Fluent<'static, &'static str> = Fluent::new(*FLUENTD_FORWARD_ADDR, *SPIFFE_ID);
}

mod errors {
    error_chain!{
        types {
            Error, ErrorKind, ResultExt, Result;
        }

        foreign_links {
             Io(::std::io::Error);
             Etcd(::etcd::Error);
             Fluent(::fruently::error::FluentError);
        }
    }
}

fn audit_event(title: &str, content: &str) {
    let mut obj: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    obj.insert(String::from(title), String::from(content));
    let fruently = fluentd_client.clone();

    std::thread::spawn(move || {
        if let Err(e) = fruently.post(obj) {
            eprintln!("{}", e);
            panic!("Cannot post audit events to fluentd");
        }
    });
}

fn main() {
    let mut router = Router::new();
    router.get("/login", login, "login");
    router.get("/get/:name", fetch_secret, "get_secret");
    router.post("/set/:name/:value", set_secret, "set_secret");

    Iron::new(router).http("0.0.0.0:3000").unwrap();
    audit_event("SERVER_START", &format!("New instance of secret-server started: {}", *SPIFFE_ID));
}

fn new_etcd_client(core: &Core) -> Result<etcd::Client<hyper::client::HttpConnector>> {
    let handle = core.handle();
    etcd::Client::new(&handle,ETCD_CLUSTER_MEMBERS.split(",").collect::<Vec<&str>>().as_slice(), None).chain_err(|| "Cannot create etcd client")
}

type AuthToken = String;

#[derive(Debug, Default)]
struct UserInfo {
    username: String,
    password: String,
    encoded_password: String,
    token: AuthToken,
}

fn fetch_user_password(user_info: &mut UserInfo) {  
    if let Ok(value) = get_etcd_key(&format!("/users/{}/password", user_info.username)) {
        user_info.encoded_password = value
    }
}

fn verify_password(user_info: &UserInfo) -> bool {
    let mut verifier = argonautica::Verifier::default();
    if let Ok(true) = verifier
        .with_hash(&user_info.encoded_password)
        .with_password(&user_info.password)
        .verify()
    {
       true
    } else {
        false
    }
}

fn login(req: &mut Request) -> IronResult<Response> {
    // Parse username and password from request
    let auth = match req.headers.get::<Authorization<Basic>>() {
        Some(auth) => auth,
        None => return Ok(Response::with(iron::status::Unauthorized))
    };

    let mut user_info = UserInfo::default();
    user_info.username = auth.username.clone();
    user_info.password = match auth.password.clone() {
        Some(password) => password,
        None  => return Ok(Response::with(iron::status::Unauthorized))
    };
    
    // Fetch user password from etcd
    fetch_user_password(&mut user_info);

    // Check password
    if !verify_password(&user_info)
    {
        audit_event("LOGIN_FAILURE_INVALID_PASSWORD", &format!("Login failure for user {} due to invalid password", user_info.username));
        return Ok(Response::with(iron::status::Unauthorized))
    }

    // Generate and set new token
    user_info.token = generate_authorization_token();
    if let Ok(_) = update_user_token(&user_info) {
        audit_event("TOKEN_CREATED", &format!("Session token {} for user {} created", user_info.token, user_info.username));
        audit_event("LOGIN_SUCCESS", &format!("Login success for user {}", user_info.username));
        Ok(Response::with((iron::status::Ok, user_info.token)))
    } else {
        audit_event("LOGIN_FAILURE_TOKEN_CREATION_FAIL", &format!("Login failure for user {} due to token creation failure", user_info.username));
        Ok(Response::with(iron::status::InternalServerError))
    }
}

fn generate_authorization_token() -> String {
    let mut rng = thread_rng();
    std::iter::repeat(())
        .map(|()| rng.sample(Alphanumeric))
        .take(24)
        .collect()
}

fn update_user_token(user_info: &UserInfo) -> Result<()> { 
    set_etcd_key(&format!("/session_tokens/{}", user_info.token), &user_info.username, Some(*TOKEN_EXPIRATION_SECS))?;
    
    Ok(())
}

fn set_secret(req: &mut Request) -> IronResult<Response> {
    // Parse name/value from URL
    let args;
    
    match req.extensions.get::<Router>()
    {
        Some(params) => args = (params.find("name").unwrap_or(""), params.find("value").unwrap_or("")),
        None => return Ok(Response::with(iron::status::BadRequest))
    };
    
    // Validate token
    let token;
    if let Some(val) = req.url.query() {
        token = val.replace("token=", "");
    } else {
        audit_event("SECRET_CREATE_FAILURE_NO_TOKEN", &format!("Secret {} failed set, no token entered attempt", args.0));
        return Ok(Response::with((iron::status::BadRequest, "Token required")));
    }

    let username;
    if let Ok(val) = validate_token(&token) {
        username = val;
    } else {
        audit_event("SECRET_CREATE_FAILURE_INVALID_TOKEN", &format!("Secret {} failed set, invalid token attempt", args.0));
        return Ok(Response::with((iron::status::Unauthorized, "Bad token")));
    }

    // Set secret
    let uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, args.0.as_bytes()); // Use secret name to gen SHA1-based UUID
    if let Err(e) = set_etcd_key(&format!("/secrets/{}/name", uuid), args.0, None) {
        eprintln!("Unable to set secret key: {}", e);
        audit_event("SECRET_CREATE_FAILURE", &format!("Unable to set secret {} by user {}, internal error", args.0, username));

        return Ok(Response::with(iron::status::InternalServerError));
    }
    if let Err(e) = set_etcd_key(&format!("/secrets/{}/value", uuid), args.1, None) {
        eprintln!("Unable to set secret value: {}", e);
        audit_event("SECRET_CREATE_FAILURE", &format!("Unable to set secret {} by user {}, internal error", args.0, username));

        return Ok(Response::with(iron::status::InternalServerError));
    }

    audit_event("SECRET_CREATE_SUCCESS", &format!("Secret {} set with UUID {} by user {}", args.0, uuid, username));
    
    Ok(Response::with((iron::status::Ok, format!("{}", uuid))))
}

fn set_etcd_key(key: &str, value: &str, expiration: Option<u64>) -> Result<()> {
    let mut core = Core::new()?;
    let client = match new_etcd_client(&core) {
        Ok(client) => client,
        Err(_) => Err("Unable to create etcd client")?
    };

    let set_token = kv::set(&client, key, value, expiration);
    core.run(set_token).or(Err(format!("Unable to update etcd key {}", key)))?;

    Ok(())
}

fn get_etcd_key(key: &str) -> Result<String> {
    let mut core = Core::new()?;
    let client = match new_etcd_client(&core) {
        Ok(client) => client,
        Err(_) => Err("Unable to create etcd client")?
    };

    let mut value = None;
    {
        let get_token = kv::get(&client, key, kv::GetOptions::default()).and_then(|response| {
            value = response.data.node.value;

            Ok(())
        });
    core.run(get_token).or(Err(format!("Unable to fetch etcd key {}", key)))?;
    }

    Ok(value.unwrap_or(String::from("")))
}

fn validate_token(token: &str) -> Result<String> {
    let mut core = Core::new()?;
    let client = match new_etcd_client(&core) {
        Ok(client) => client,
        Err(_) => Err("Unable to create etcd client")?
    };

    let mut username = None;
    {
    let fetch_token = kv::get(&client, &format!("/session_tokens/{}", token), kv::GetOptions::default()).and_then(|response| {
        username = response.data.node.value;

        Ok(())
    });
    core.run(fetch_token).or(Err(format!("Token {} not found", token)))?;
    }
    
    Ok(username.unwrap_or(String::from("")))
}

fn fetch_secret(req: &mut Request) -> IronResult<Response> {
    // Parse name from URL
    let name;
    
    match req.extensions.get::<Router>()
    {
        Some(params) => name = params.find("name").unwrap_or(""),
        None => return Ok(Response::with(iron::status::BadRequest)) // This should never happen
    };
    
    // Validate token
    let token;
    if let Some(val) = req.url.query() {
        token = val.replace("token=", "");
    } else {
        audit_event("SECRET_FETCH_FAILURE_NO_TOKEN", &format!("Secret {} failed fetch, no token entered attempt", name));
        return Ok(Response::with((iron::status::BadRequest, "Token required")));
    }

    let username;
    if let Ok(val) = validate_token(&token) {
        username = val;
    } else {
        audit_event("SECRET_FETCH_FAILURE_INVALID_TOKEN", &format!("Secret {} failed fetch, invalid token attempt", name));
        return Ok(Response::with((iron::status::Unauthorized, "Bad token")));
    }

    // Fetch secret
    let uuid = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes());
    let value = get_etcd_key(&format!("/secrets/{}/value", uuid));
    match value {
        Ok(value) => {
            audit_event("SECRET_FETCH_SUCCESS", &format!("Secret {} UUID {} fetched by user {}", name, uuid, username));
            Ok(Response::with((iron::status::Ok, value)))
        }, 
        Err(e) => {
            eprintln!("Unable to fetch secret: {}", e);
            audit_event("SECRET_FETCH_FAILURE_NOEXIST", &format!("Secret {} failed fetch by user {}, does not exist", name, username));
            Ok(Response::with((iron::status::BadRequest, "Invalid secret")))
        }
    }
}
