// Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0

use lambda_extension::{service_fn as extension_handler, Extension};
use lambda_http::{service_fn as http_handler, Body, Request, Response};
use log::*;
use reqwest::{redirect, Client};
use std::{env, mem};
use tokio::runtime::Handle;
use tokio_retry::{strategy::FixedInterval, Retry};

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    env_logger::init();

    // register as an external extension
    let handle = Handle::current();
    tokio::task::spawn_blocking(move || {
        handle.spawn(async {
            Extension::new()
                .with_events(&[])
                .with_events_processor(extension_handler(|_| async { Ok::<(), Error>(()) }))
                .run()
                .await
                .expect("extension thread error");
        })
    });

    // check if the application is ready every 10 milliseconds
    Retry::spawn(FixedInterval::from_millis(10), || {
        let readiness_check_url = format!(
            "http://127.0.0.1:{}{}",
            env::var("READINESS_CHECK_PORT").unwrap_or_else(|_| "8080".to_string()),
            env::var("READINESS_CHECK_PATH").unwrap_or_else(|_| "/".to_string())
        );
        reqwest::get(readiness_check_url)
    })
    .await?;

    // start lambda runtime
    let options = &HandlerOptions {
        http_client: Client::builder().redirect(redirect::Policy::none()).build().unwrap(),
        host: env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string()),
        port: env::var("PORT").unwrap_or_else(|_| "8080".to_string()),
        base_path: env::var("REMOVE_BASE_PATH").unwrap_or_default(),
    };
    lambda_http::run(http_handler(move |event: Request| async move { http_proxy_handler(event, options).await })).await?;
    Ok(())
}

struct HandlerOptions {
    http_client: Client,
    host: String,
    port: String,
    base_path: String,
}

async fn http_proxy_handler(event: Request, options: &HandlerOptions) -> Result<Response<Body>, Error> {
    let host = options.host.as_str();
    let port = options.port.as_str();
    let (parts, body) = event.into_parts();
    let mut path_and_query = parts.uri.path_and_query().unwrap().as_str();
    // strip away Base Path if environment variable REMOVE_BASE_PATH is set.
    if options.base_path != String::default() {
        if let Some(value) = path_and_query.strip_prefix(options.base_path.as_str()) {
            path_and_query = value;
        };
    }
    let app_url = format!("http://{}:{}{}", host, port, path_and_query);
    debug!("app_url is {:#?}", app_url);

    let app_response = options
        .http_client
        .request(parts.method, app_url)
        .headers(parts.headers)
        .body(body.to_vec())
        .send()
        .await?;

    let mut lambda_response = Response::builder();
    let _ = mem::replace(lambda_response.headers_mut().unwrap(), app_response.headers().clone());
    Ok(lambda_response
        .status(app_response.status())
        .body(convert_body(app_response).await?)
        .expect("failed to send response"))
}

async fn convert_body(app_response: reqwest::Response) -> Result<Body, Error> {
    debug!("app response headers are {:#?}", app_response.headers());

    if app_response.headers().get(http::header::CONTENT_ENCODING).is_some() {
        let content = app_response.bytes().await?;
        return Ok(Body::Binary(content.to_vec()));
    }

    let content_type = if let Some(value) = app_response.headers().get(http::header::CONTENT_TYPE) {
        value.to_str().unwrap_or_default()
    } else {
        ""
    };

    if content_type.starts_with("text")
        || content_type.starts_with("application/json")
        || content_type.starts_with("application/javascript")
        || content_type.starts_with("application/xml")
    {
        let body_text = app_response.text().await?;
        return Ok(Body::Text(body_text));
    }
    let content = app_response.bytes().await?;
    if !content.is_empty() {
        Ok(Body::Binary(content.to_vec()))
    } else {
        Ok(Body::Empty)
    }
}
