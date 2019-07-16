#![allow(proc_macro_derive_resolution_fallback)] // See: https://github.com/diesel-rs/diesel/issues/1785

extern crate actix_cors;
extern crate actix_rt;
extern crate actix_web;
extern crate futures;
extern crate serde;
extern crate serde_json;
#[macro_use]
extern crate serde_derive;
#[macro_use]
extern crate diesel;
extern crate authy;
extern crate chrono;
extern crate dotenv;
extern crate env_logger;
#[macro_use]
extern crate derive_more;
#[macro_use]
extern crate log;
extern crate regex;
#[macro_use]
extern crate lazy_static;
extern crate textnonce;

mod api;
mod db;
mod error;
mod models;
mod schema;

use self::api::{ApiResponse, ResponseStatus};
use self::db::{
    BeerSearchResult, BrewerySearchResult, Connection, CreateBeer, CreateBrewery, CreateDrink,
    DeleteDrink, ExpandedDrink, GetBeerByName, GetBreweryByName, GetDrink, GetDrinks,
    LookupIdentiy, Pool, SearchBeerByName, SearchBreweryByName, StartSession,
};

use std::convert::From;
use std::str::FromStr;

use actix_cors::Cors;
use actix_web::middleware::Logger;
use actix_web::*;
use actix_web::{App, HttpRequest, HttpServer, Responder};
use authy::AuthyError;
use chrono::naive::NaiveDate;
use diesel::prelude::*;
use diesel::r2d2::ConnectionManager;
use futures::future::Either;
use futures::Future;
use regex::Regex;

fn index() -> impl Responder {
    #[derive(Serialize)]
    #[serde(rename = "message")]
    struct TestResponse(String);

    HttpResponse::Ok().json(ApiResponse::success(TestResponse("Hello world!".into())))
}

// Dummy method. Just wanted a route for the front-end to ping to make up the heroku instance.
fn wakeup() -> impl Responder {
    #[derive(Serialize)]
    #[serde(rename = "message")]
    struct TestResponse(String);

    HttpResponse::Ok().json(ApiResponse::success(TestResponse("👍".into())))
}

fn get_drinks(
    pool: web::Data<Pool>,
    person: models::Person,
) -> impl Future<Item = HttpResponse, Error = Error> {
    #[derive(Serialize)]
    #[serde(rename = "drinks")]
    struct Drinks(Vec<ExpandedDrink>);

    db::execute(
        &pool,
        GetDrinks {
            person_id: person.id,
        },
    )
    .from_err()
    .and_then(|res| match res {
        Ok(drinks) => Ok(HttpResponse::Ok().json(ApiResponse::success(Drinks(drinks)))),
        Err(_) => Ok(HttpResponse::InternalServerError().into()),
    })
}

#[derive(Deserialize)]
struct DrinkForm {
    /// Date on which the drink was had.
    drank_on: NaiveDate,

    /// The name of the beer.
    beer: String,

    /// The name of the beer's brewery.
    brewery: String,

    /// Rating of the beer.
    rating: i16,

    /// A comment/opinion about the beer.
    comment: Option<String>,
}

/// Route handler for creating new drink records
///
/// Requires a valid session token in the `Authorization` header.
///
/// Expects the following POST data:
///
/// - `drank_on`: The date on which the drink was had (yyyy-mm-dd).
/// - `beer`: The name of the beer
/// - `brewery`: The name of the brewery
/// - `rating`: The rating of the beer, 0 - 5
/// - `comment`: An optional comment about the beer
///
/// If no records correspond to the `beer` or `brewery` names, new records will be created.
fn new_drink(
    pool: web::Data<Pool>,
    person: models::Person,
    details: web::Form<DrinkForm>,
) -> impl Future<Item = HttpResponse, Error = Error> {
    // Save these for later
    let beer_name = details.beer.clone();

    /*********************************************/
    /*  Closures for database operations         */
    /*********************************************/

    // This closure will create a new brewery record with the given `name`.
    let create_brewery = |pool: &Pool, name: String| {
        db::execute(pool, CreateBrewery { name: name })
            .from_err()
            .and_then(|res| res)
            .map_err(|e| actix_web::Error::from(e))
    };

    // This closure will create a new beer record, given a `name` and its `brewery_id`.
    let create_beer = |pool: &Pool, name: String, brewery_id: i32| {
        db::execute(pool, CreateBeer { name, brewery_id })
            .from_err()
            .and_then(|res| res)
            .map_err(|e| actix_web::Error::from(e))
    };

    // This closure will lookup a brewery given its `name` and,
    // if no matching record is found, will insert a new one.
    let get_brewery = |pool: &Pool, name: String| {
        let pool_clone = pool.clone();
        db::execute(pool, GetBreweryByName { name: name.clone() })
            .from_err::<Error>()
            .map(move |res| match res {
                Ok(Some(brewery)) => Either::A(futures::future::result(Ok(brewery))),
                Ok(None) => Either::B(create_brewery(&pool_clone, name)),
                Err(e) => Either::A(futures::future::result(Err(actix_web::Error::from(e)))),
            })
            .from_err::<actix_web::Error>()
            .flatten()
    };

    // This closure will lookup a beer given its `name` and `brewery_id` and,
    // will insert a new one if no record is found.
    let get_beer = move |pool: &Pool, name: String, brewery_id: i32| {
        let pool_clone = pool.clone();
        db::execute(
            pool,
            GetBeerByName {
                name: name.clone(),
                brewery_id: brewery_id,
            },
        )
        .from_err()
        .and_then(move |res| match res {
            Ok(Some(beer)) => Either::A(futures::future::result(Ok(beer))),
            Ok(None) => Either::B(create_beer(&pool_clone, name, brewery_id)),
            Err(e) => Either::A(futures::future::result(Err(actix_web::Error::from(e)))),
        })
    };

    // This will insert a new Drink record
    let record_drink = |pool: &Pool, drink: CreateDrink| {
        db::execute(pool, drink)
            .from_err()
            .and_then(|res| res)
            .map_err(|e| actix_web::Error::from(e))
    };

    // Get an ExpandedDrink record by ID
    let get_drink = |pool: &Pool, drink_id: i32| {
        db::execute(pool, GetDrink { drink_id })
            .from_err()
            .and_then(|res| res)
            .map_err(|e| actix_web::Error::from(e))
    };

    /*********************************************/
    /* Begin actual function execution           */
    /*********************************************/

    let pool_clone_1 = pool.clone();
    let pool_clone_2 = pool.clone();

    // Look up the given brewery, and create a new record if one is not found
    get_brewery(&pool, details.brewery.clone())
        // Then lookup the beer by name, and create a new record if it is not found.
        .and_then(move |brewery| get_beer(&pool, beer_name, brewery.id))
        // Finally, insert a record of the individual drink
        .and_then(move |beer| {
            let drink = CreateDrink {
                person_id: person.id,
                drank_on: details.drank_on,
                beer_id: beer.id,
                rating: details.rating,
                comment: details.comment.clone(),
            };

            record_drink(&pool_clone_1, drink)
        })
        .and_then(move |drink| get_drink(&pool_clone_2, drink.id))
        // Format the result for output
        .then(|res| match res {
            Ok(drink) => Ok(HttpResponse::Ok().json(ApiResponse::success(drink))),
            Err(_) => Ok(HttpResponse::InternalServerError().into()),
        })
}

#[derive(Deserialize)]
struct DrinkIdForm {
    id: i32,
}

fn delete_drink(
    person: models::Person,
    info: web::Path<DrinkIdForm>,
    pool: web::Data<Pool>,
) -> impl Future<Item = HttpResponse, Error = Error> {
    db::execute(
        &pool,
        DeleteDrink {
            drink_id: info.id,
            person_id: person.id,
        },
    )
    .from_err()
    .and_then(move |res| match res {
        Ok(0) => {
            let not_found = ApiResponse::<()>::from(None)
                .with_status(ResponseStatus::Fail)
                .add_message("Could not find that drink".into());

            Ok(HttpResponse::NotFound().json(not_found))
        }
        Ok(1) => {
            let deleted = ApiResponse::<()>::from(None)
                .with_status(ResponseStatus::Success)
                .add_message("Deleted".into());

            Ok(HttpResponse::Ok().json(deleted))
        }
        Ok(n) => {
            error!("Person {} somehow deleted {} drinks!", person.id, n);

            let unexpected_error = ApiResponse::<()>::from(None)
                .with_status(ResponseStatus::Error)
                .add_message("An unexpected error occurred".into());

            Ok(HttpResponse::InternalServerError().json(unexpected_error))
        }
        Err(e) => {
            error!(
                "Unable to delete drink for person {}! Error: {}",
                person.id, e
            );

            let unexpected_error = ApiResponse::<()>::from(None)
                .with_status(ResponseStatus::Error)
                .add_message("An unexpected error occurred".into());

            Ok(HttpResponse::InternalServerError().json(unexpected_error))
        }
    })
}

#[derive(Deserialize)]
struct AuthForm {
    country_code: u16,
    phone_number: String,
    code: Option<String>,
}

fn begin_auth(form: web::Form<AuthForm>) -> impl Future<Item = HttpResponse, Error = Error> {
    use authy::api::phone;

    lazy_static! {
        // See: https://github.com/authy/authy-form-helpers/blob/be2081cd44041ba61173658c100471c8ff7302b9/src/form.authy.js#L693
        static ref RE: Regex =
            Regex::new(r"^([0-9][0-9][0-9])\W*([0-9][0-9]{2})\W*([0-9]{0,5})$").unwrap();
    }

    // Check to make sure that the identity submitted appears to be a phone number
    if !RE.is_match(&form.phone_number) {
        info!(
            "Received invalid phone number '{}' '{}'!",
            form.country_code, form.phone_number
        );

        let response = ApiResponse::<()>::from(None)
            .with_status(ResponseStatus::Fail)
            .add_message("Invalid phone number".into());

        return futures::future::ok(HttpResponse::BadRequest().json(response));
    }

    let client = authy::Client::new(
        "https://api.authy.com",
        &std::env::var("AUTHY_API_KEY").expect("An authy API key is required!"),
    );

    let (status, _start) = match phone::start(
        &client,
        phone::ContactType::SMS,
        form.country_code,
        &form.phone_number,
        Some(6),
        None,
    ) {
        Ok(res) => res,
        Err(e) => {
            error!("Failed to start phone number verification! Error: {}", e);

            let response = ApiResponse::<()>::from(None)
                .with_status(ResponseStatus::Error)
                .add_message("That phone number didn't work :(".into());

            return futures::future::ok(HttpResponse::BadRequest().json(response));
        }
    };

    let response = ApiResponse::<()>::from(None).add_message(status.message);

    futures::future::ok(HttpResponse::Ok().json(response))
}

fn complete_auth(
    form: web::Form<AuthForm>,
    pool: web::Data<Pool>,
) -> impl Future<Item = HttpResponse, Error = Error> {
    use authy::api::phone;

    /*********************************************/
    /*  Closures for database operations         */
    /*********************************************/

    let lookup_idenity = |pool: &Pool, country_code: u16, phone_number: String| {
        db::execute(
            pool,
            LookupIdentiy {
                identifier: format!("{}{}", country_code, phone_number),
            },
        )
        .from_err()
        .and_then(|res| res)
        .map_err(|e| actix_web::Error::from(e))
    };

    let start_session = |pool: &Pool, person_id: i32| {
        db::execute(pool, StartSession { person_id })
            .from_err()
            .and_then(|res| res)
            .map_err(|e| actix_web::Error::from(e))
    };

    /*********************************************/
    /*  Begin request handling logic             */
    /*********************************************/

    lazy_static! {
        // See: https://github.com/authy/authy-form-helpers/blob/be2081cd44041ba61173658c100471c8ff7302b9/src/form.authy.js#L693
        static ref RE: Regex =
            Regex::new(r"^([0-9][0-9][0-9])\W*([0-9][0-9]{2})\W*([0-9]{0,5})$").unwrap();
    }

    // Make sure some kind of verification code was submitted
    if form.code.is_none() {
        info!("Verification code was submitted!");

        let response = ApiResponse::<()>::from(None)
            .with_status(ResponseStatus::Fail)
            .add_message("Missing verification code!".into());

        return Either::A(futures::future::ok(
            HttpResponse::BadRequest().json(response),
        ));
    }

    // Check to make sure that the identity submitted appears to be a phone number
    if !RE.is_match(&form.phone_number) {
        info!(
            "Received invalid phone number '{}' '{}'!",
            form.country_code, form.phone_number
        );

        let response = ApiResponse::<()>::from(None)
            .with_status(ResponseStatus::Fail)
            .add_message("Invalid phone number!".into());

        return Either::A(futures::future::ok(
            HttpResponse::BadRequest().json(response),
        ));
    }

    /*********************************************/
    /*  Verify the phone number and code         */
    /*********************************************/

    let client = authy::Client::new(
        "https://api.authy.com",
        &std::env::var("AUTHY_API_KEY").expect("An authy API key is required!"),
    );

    let verification_code = form.code.clone().unwrap_or("wtf".into());

    // Attempt to verify the verification code
    let verification_status = phone::check(
        &client,
        form.country_code,
        &form.phone_number,
        &verification_code,
    );

    match verification_status {
        Ok(status) => {
            // If the verification code was invalid, return an error
            if !status.success {
                warn!(
                    "Invalid verification code, '{}', submitted for '{}' '{}'!",
                    verification_code, form.country_code, form.phone_number
                );

                let response = ApiResponse::<()>::from(None)
                    .with_status(ResponseStatus::Fail)
                    .add_message("Invalid verification code".into());

                return Either::A(futures::future::ok(
                    HttpResponse::Forbidden().json(response),
                ));
            }

            // Verification was correct
            info!(
                "Phone number {} {} verified!",
                form.country_code, form.phone_number
            );
        }
        Err(e) => {
            return match e {
                // If there was an internal error, that the Authy crate has bubbled up.
                AuthyError::RequestError(e)
                | AuthyError::IoError(e)
                | AuthyError::JsonParseError(e) => {
                    // Something awful happened
                    warn!(
                        "Unable to verify code, '{}', submitted for '{}' '{}'! Error: {}",
                        verification_code, form.country_code, form.phone_number, e
                    );

                    let response = ApiResponse::<()>::from(None)
                        .with_status(ResponseStatus::Error)
                        .add_message("Internal server error".into());

                    Either::A(futures::future::ok(
                        HttpResponse::InternalServerError().json(response),
                    ))
                }
                // If the verification code was incorrect
                // The Authy crate currently returns this as an Unauthorized API Key error.
                AuthyError::UnauthorizedKey(_) => {
                    warn!(
                        "Invalid verification code, '{}', submitted for '{}' '{}'!",
                        verification_code, form.country_code, form.phone_number
                    );

                    let response = ApiResponse::<()>::from(None)
                        .with_status(ResponseStatus::Fail)
                        .add_message("Invalid verification code".into());

                    Either::A(futures::future::ok(
                        HttpResponse::Forbidden().json(response),
                    ))
                }
                // If we received some other Authy error response.
                e => {
                    warn!(
                        "Unexpected authy error during verification, '{}', submitted for '{}' '{}'! Error: {}",
                        verification_code, form.country_code, form.phone_number, e
                    );

                    let response = ApiResponse::<()>::from(None)
                        .with_status(ResponseStatus::Fail)
                        .add_message("Unable to verify the code".into());

                    Either::A(futures::future::ok(
                        HttpResponse::Forbidden().json(response),
                    ))
                }
            };
        }
    }

    /*********************************************/
    /*  Verified, find identity, start session   */
    /*********************************************/

    let pool_clone = pool.clone();

    Either::B(
        lookup_idenity(&pool, form.country_code, form.phone_number.clone())
            .and_then(move |ident| start_session(&pool_clone, ident.person_id))
            .then(move |res| match res {
                Ok(session) => {
                    info!(
                        "Successfully verified identity for person {}",
                        session.person_id
                    );

                    Ok(HttpResponse::Ok().json(ApiResponse::success(session)))
                }
                Err(e) => {
                    error!("Failed to start session! Error: {}", e);

                    let response = ApiResponse::<()>::from(None)
                        .with_status(ResponseStatus::Error)
                        .add_message("Internal server error".into());

                    Ok(HttpResponse::InternalServerError().json(response))
                }
            }),
    )
}

fn test_auth(person: models::Person) -> impl Responder {
    #[derive(Serialize)]
    #[serde(rename = "message")]
    struct TestResponse(String);

    HttpResponse::Ok().json(ApiResponse::success(TestResponse(format!(
        "Hello person {}",
        person.id
    ))))
}

#[derive(Deserialize)]
struct SearchForm {
    query: String,
}

fn search_beer(
    search_form: web::Query<SearchForm>,
    pool: web::Data<Pool>,
) -> impl Future<Item = HttpResponse, Error = Error> {
    #[derive(Serialize)]
    #[serde(rename = "beers")]
    struct SearchResults(Vec<BeerSearchResult>);

    // If the `query` is empty, then return an error
    if search_form.query.trim().is_empty() {
        let response = ApiResponse::<()>::from(None)
            .with_status(ResponseStatus::Fail)
            .add_message("Empty search query".into());

        return Either::A(futures::future::ok(
            HttpResponse::BadRequest().json(response),
        ));
    }

    Either::B(
        db::execute(
            &pool,
            SearchBeerByName {
                query: search_form.query.clone(),
            },
        )
        .from_err()
        .and_then(|res| match res {
            Ok(beers) => Ok(HttpResponse::Ok().json(ApiResponse::success(SearchResults(beers)))),
            Err(e) => {
                error!("{}", e);
                Ok(HttpResponse::InternalServerError().into())
            }
        }),
    )
}

fn search_brewery(
    search_form: web::Query<SearchForm>,
    pool: web::Data<Pool>,
) -> impl Future<Item = HttpResponse, Error = Error> {
    #[derive(Serialize)]
    #[serde(rename = "breweries")]
    struct SearchResults(Vec<BrewerySearchResult>);

    // If the `query` is empty, then return an error
    if search_form.query.trim().is_empty() {
        let response = ApiResponse::<()>::from(None)
            .with_status(ResponseStatus::Fail)
            .add_message("Empty search query".into());

        return Either::A(futures::future::ok(
            HttpResponse::BadRequest().json(response),
        ));
    }

    Either::B(
        db::execute(
            &pool,
            SearchBreweryByName {
                query: search_form.query.clone(),
            },
        )
        .from_err()
        .and_then(|res| match res {
            Ok(breweries) => {
                Ok(HttpResponse::Ok().json(ApiResponse::success(SearchResults(breweries))))
            }
            Err(e) => {
                error!("{}", e);
                Ok(HttpResponse::InternalServerError().into())
            }
        }),
    )
}

fn main() {
    dotenv::dotenv().ok();
    env_logger::init();

    // Make sure an authy API key is set before starting.
    let _ = std::env::var("AUTHY_API_KEY").expect("An authy API key is required!");

    // Read the port on which to listen.
    let port = u16::from_str(&std::env::var("PORT").unwrap_or("1234".into()))
        .expect("Failed to parse $PORT!");

    // Read the IP address on which to listen
    let ip = std::net::IpAddr::from_str(&std::env::var("LISTEN_IP").unwrap_or("127.0.0.1".into()))
        .expect("Failed to parse $LISTEN_IP");

    // Construct the full Socket address
    let listen_addr = std::net::SocketAddr::new(ip, port);

    // Create a connection pool to the database
    let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set!");
    let manager = ConnectionManager::<PgConnection>::new(database_url);
    let pool = Pool::new(manager).expect("Failed to create database connection pool!");

    let sys = actix_rt::System::new("http-server");

    HttpServer::new(move || {
        App::new()
            .data(pool.clone())
            .wrap(Logger::default())
            .wrap(Cors::default())
            .route("/", web::get().to(index))
            .route("/wakeup", web::get().to(wakeup))
            .service(
                web::scope("/drink")
                    .service(
                        web::resource("")
                            .route(web::get().to_async(get_drinks))
                            .route(web::post().to_async(new_drink)),
                    )
                    .service(web::resource("/{id}").route(web::delete().to_async(delete_drink))),
            )
            .service(
                web::scope("/auth")
                    .service(web::resource("").route(web::post().to_async(begin_auth)))
                    .service(web::resource("/verify").route(web::post().to_async(complete_auth)))
                    .service(web::resource("/test").route(web::get().to(test_auth))),
            )
            .service(
                web::scope("/search")
                    .service(web::resource("/beer").route(web::get().to_async(search_beer)))
                    .service(web::resource("/brewery").route(web::get().to_async(search_brewery))),
            )
    })
    .bind(&listen_addr)
    .unwrap()
    .start();

    info!("Listening on {}", listen_addr);

    let _ = sys.run();
}
