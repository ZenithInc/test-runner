use anyhow::{Context, Result, bail};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use jsonwebtoken::{EncodingKey, Header};
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, Row, mysql::MySqlPoolOptions};
use std::{
    env,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{
    net::TcpListener,
    time::{Duration, sleep},
};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 3000;
const DEFAULT_JWT_SECRET: &str = "sample-development-secret";
const DATABASE_RETRIES: u32 = 20;
const REDIS_RETRIES: u32 = 20;
const SERVICE_RETRY_DELAY: Duration = Duration::from_secs(1);
const TOKEN_TTL_SECONDS: u64 = 3600;
const SMS_CODE_TTL_SECONDS: u64 = 300;
const TOKEN_KEY_PREFIX: &str = "auth:token:";
const SMS_CODE_KEY_PREFIX: &str = "sms:code:";

#[derive(Clone)]
struct AppState {
    database: Option<MySqlPool>,
    redis: Option<redis::Client>,
    jwt_secret: String,
    sms_provider_base_url: Option<String>,
    http_client: reqwest::Client,
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

#[derive(Debug, Deserialize)]
struct RegisterRequest {
    name: String,
    email: String,
    password: String,
}

#[derive(Debug, Serialize)]
struct RegisterResponse {
    id: u64,
    name: String,
    email: String,
}

#[derive(Debug, Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
    phone: String,
    sms_code: String,
}

#[derive(Debug, Serialize)]
struct LoginResponse {
    access_token: String,
    token_type: &'static str,
    expires_in: u64,
}

#[derive(Debug, Deserialize)]
struct SendSmsCodeRequest {
    phone: String,
    #[serde(default)]
    provider_base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CreateOrderRequest {
    customer: OrderCustomerRequest,
    items: Vec<OrderItemRequest>,
    #[serde(default)]
    coupon_code: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrderCustomerRequest {
    name: String,
    email: String,
    #[serde(default)]
    tier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrderItemRequest {
    sku: String,
    quantity: u32,
    unit_price: u64,
}

#[derive(Debug, Serialize)]
struct CreateOrderResponse {
    order_id: String,
    status: &'static str,
    customer: OrderCustomerResponse,
    items: Vec<OrderItemResponse>,
    pricing: OrderPricingResponse,
    flags: OrderFlagsResponse,
    metadata: OrderMetadataResponse,
}

#[derive(Debug, Serialize)]
struct OrderCustomerResponse {
    name: String,
    email: String,
    tier: Option<String>,
}

#[derive(Debug, Serialize)]
struct OrderItemResponse {
    sku: String,
    quantity: u32,
    unit_price: u64,
    line_total: u64,
}

#[derive(Debug, Serialize)]
struct OrderPricingResponse {
    sku_count: u64,
    item_count: u64,
    subtotal: u64,
    discount: u64,
    shipping_fee: u64,
    payable_total: u64,
}

#[derive(Debug, Serialize)]
struct OrderFlagsResponse {
    has_discount: bool,
    has_free_shipping: bool,
}

#[derive(Debug, Serialize)]
struct OrderMetadataResponse {
    coupon_code: Option<String>,
    note: Option<String>,
}

#[derive(Debug, Serialize)]
struct SendSmsCodeResponse {
    phone: String,
    code: String,
    provider: String,
    provider_request_id: String,
}

#[derive(Debug, Serialize)]
struct SmsProviderRequest {
    phone: String,
    message: String,
}

#[derive(Debug, Deserialize)]
struct SmsProviderResponse {
    accepted: bool,
    provider: String,
    request_id: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Debug, Serialize)]
struct JwtClaims {
    sub: String,
    email: String,
    exp: usize,
    iat: usize,
}

#[derive(Debug)]
struct StoredUser {
    id: u64,
    email: String,
    password_hash: String,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let host = env::var("HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string());
    let port = parse_port()?;
    let database = configure_database().await?;
    let redis = configure_redis().await?;
    let http_client = reqwest::Client::builder()
        .build()
        .context("failed to build outbound http client")?;

    let listener = TcpListener::bind((host.as_str(), port))
        .await
        .with_context(|| format!("failed to bind {host}:{port}"))?;

    let state = AppState {
        database,
        redis,
        jwt_secret: env::var("JWT_SECRET").unwrap_or_else(|_| DEFAULT_JWT_SECRET.to_string()),
        sms_provider_base_url: env::var("SMS_PROVIDER_BASE_URL")
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty()),
        http_client,
    };
    let app = Router::new()
        .route("/health", get(health))
        .route("/orders", post(create_order))
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/send-sms-code", post(send_sms_code))
        .with_state(state);

    println!("health-service listening on http://{host}:{port}");

    axum::serve(listener, app)
        .await
        .context("health-service server exited unexpectedly")
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "health-service",
    })
}

async fn create_order(
    Json(payload): Json<CreateOrderRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let customer = normalize_order_customer(payload.customer)?;
    let items = normalize_order_items(payload.items)?;
    let coupon_code = normalize_coupon_code(payload.coupon_code);
    let note = normalize_optional_text(payload.note);
    let pricing = build_order_pricing(&items, coupon_code.as_deref(), customer.tier.as_deref())?;

    Ok((
        StatusCode::CREATED,
        Json(CreateOrderResponse {
            order_id: issue_order_id()?,
            status: "created",
            customer,
            items,
            flags: OrderFlagsResponse {
                has_discount: pricing.discount > 0,
                has_free_shipping: pricing.shipping_fee == 0,
            },
            metadata: OrderMetadataResponse { coupon_code, note },
            pricing,
        }),
    ))
}

async fn register(
    State(state): State<AppState>,
    Json(payload): Json<RegisterRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(pool) = state.database.as_ref() else {
        return Err(ApiError::service_unavailable(
            "database is not configured; start the sample with docker compose or set DATABASE_URL",
        ));
    };

    let name = normalize_name(payload.name)?;
    let email = normalize_email(payload.email)?;
    validate_password(&payload.password)?;
    let password_hash = hash_password(&payload.password)?;

    let result = sqlx::query("INSERT INTO users (name, email, password_hash) VALUES (?, ?, ?)")
        .bind(&name)
        .bind(&email)
        .bind(&password_hash)
        .execute(pool)
        .await
        .map_err(map_database_error)?;

    Ok((
        StatusCode::CREATED,
        Json(RegisterResponse {
            id: result.last_insert_id(),
            name,
            email,
        }),
    ))
}

async fn login(
    State(state): State<AppState>,
    Json(payload): Json<LoginRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(pool) = state.database.as_ref() else {
        return Err(ApiError::service_unavailable(
            "database is not configured; start the sample with docker compose or set DATABASE_URL",
        ));
    };
    let Some(redis) = state.redis.as_ref() else {
        return Err(ApiError::service_unavailable(
            "redis is not configured; start the sample with docker compose or set REDIS_URL",
        ));
    };

    let email = normalize_email(payload.email)?;
    ensure_password_present(&payload.password)?;
    let phone = normalize_phone(payload.phone)?;
    let sms_code = normalize_sms_code(payload.sms_code)?;

    let Some(user) = find_user_by_email(pool, &email).await? else {
        return Err(ApiError::unauthorized("invalid email or password"));
    };

    if !verify_password(&payload.password, &user.password_hash)? {
        return Err(ApiError::unauthorized("invalid email or password"));
    }

    consume_verification_code(redis, &phone, &sms_code).await?;
    let access_token = issue_token(user.id, &user.email, &state.jwt_secret)?;
    store_token(redis, &access_token, &user.email).await?;

    Ok((
        StatusCode::OK,
        Json(LoginResponse {
            access_token,
            token_type: "Bearer",
            expires_in: TOKEN_TTL_SECONDS,
        }),
    ))
}

async fn send_sms_code(
    State(state): State<AppState>,
    Json(payload): Json<SendSmsCodeRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(redis) = state.redis.as_ref() else {
        return Err(ApiError::service_unavailable(
            "redis is not configured; start the sample with docker compose or set REDIS_URL",
        ));
    };

    let phone = normalize_phone(payload.phone)?;
    let provider_base_url = resolve_sms_provider_base_url(payload.provider_base_url, &state)?;
    let code = generate_verification_code()?;
    let provider_response =
        call_sms_provider(&state.http_client, &provider_base_url, &phone, &code).await?;

    if !provider_response.accepted {
        return Err(ApiError::bad_gateway("sms provider rejected the request"));
    }

    store_verification_code(redis, &phone, &code).await?;

    Ok((
        StatusCode::OK,
        Json(SendSmsCodeResponse {
            phone,
            code,
            provider: provider_response.provider,
            provider_request_id: provider_response.request_id,
        }),
    ))
}

fn parse_port() -> Result<u16> {
    match env::var("PORT") {
        Ok(raw) => raw
            .parse::<u16>()
            .with_context(|| format!("PORT must be a valid u16, got `{raw}`")),
        Err(env::VarError::NotPresent) => Ok(DEFAULT_PORT),
        Err(err) => Err(err.into()),
    }
}

async fn configure_database() -> Result<Option<MySqlPool>> {
    let database_url = match env::var("DATABASE_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        Ok(_) | Err(env::VarError::NotPresent) => {
            eprintln!(
                "DATABASE_URL is not set; /register and /login will return 503 until a database is configured"
            );
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };

    let pool = connect_database_with_retry(&database_url).await?;
    ensure_schema(&pool).await?;
    Ok(Some(pool))
}

async fn configure_redis() -> Result<Option<redis::Client>> {
    let redis_url = match env::var("REDIS_URL") {
        Ok(url) if !url.trim().is_empty() => url,
        Ok(_) | Err(env::VarError::NotPresent) => {
            eprintln!(
                "REDIS_URL is not set; /login and /send-sms-code will return 503 until Redis is configured"
            );
            return Ok(None);
        }
        Err(err) => return Err(err.into()),
    };

    let client = connect_redis_with_retry(&redis_url).await?;
    Ok(Some(client))
}

async fn connect_database_with_retry(database_url: &str) -> Result<MySqlPool> {
    let mut last_error = None;

    for attempt in 1..=DATABASE_RETRIES {
        match MySqlPoolOptions::new()
            .max_connections(5)
            .connect(database_url)
            .await
        {
            Ok(pool) => return Ok(pool),
            Err(error) => {
                last_error = Some(error);
                if attempt < DATABASE_RETRIES {
                    eprintln!(
                        "database is not ready yet (attempt {attempt}/{DATABASE_RETRIES}), retrying..."
                    );
                    sleep(SERVICE_RETRY_DELAY).await;
                }
            }
        }
    }

    Err(last_error
        .expect("database retry loop should capture an error")
        .into())
}

async fn connect_redis_with_retry(redis_url: &str) -> Result<redis::Client> {
    let client = redis::Client::open(redis_url)
        .with_context(|| format!("REDIS_URL must be a valid Redis URL, got `{redis_url}`"))?;
    let mut last_error = None;

    for attempt in 1..=REDIS_RETRIES {
        match client.get_multiplexed_tokio_connection().await {
            Ok(mut connection) => {
                let ping: redis::RedisResult<String> =
                    redis::cmd("PING").query_async(&mut connection).await;
                match ping {
                    Ok(_) => return Ok(client.clone()),
                    Err(error) => last_error = Some(error.to_string()),
                }
            }
            Err(error) => {
                last_error = Some(error.to_string());
            }
        }

        if attempt < REDIS_RETRIES {
            eprintln!("redis is not ready yet (attempt {attempt}/{REDIS_RETRIES}), retrying...");
            sleep(SERVICE_RETRY_DELAY).await;
        }
    }

    bail!(
        "failed to connect to redis after {REDIS_RETRIES} attempts: {}",
        last_error.unwrap_or_else(|| "unknown redis error".to_string())
    )
}

async fn ensure_schema(pool: &MySqlPool) -> Result<()> {
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS users (
            id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,
            name VARCHAR(100) NOT NULL,
            email VARCHAR(255) NOT NULL,
            password_hash VARCHAR(255) NOT NULL,
            created_at TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
            UNIQUE KEY uq_users_email (email)
        ) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci
        "#,
    )
    .execute(pool)
    .await
    .context("failed to ensure users table exists")?;

    Ok(())
}

async fn find_user_by_email(pool: &MySqlPool, email: &str) -> Result<Option<StoredUser>, ApiError> {
    let row = sqlx::query("SELECT id, email, password_hash FROM users WHERE email = ?")
        .bind(email)
        .fetch_optional(pool)
        .await
        .map_err(map_database_error)?;

    let Some(row) = row else {
        return Ok(None);
    };

    Ok(Some(StoredUser {
        id: row
            .try_get("id")
            .map_err(|error| ApiError::internal(format!("failed to read user id: {error}")))?,
        email: row
            .try_get("email")
            .map_err(|error| ApiError::internal(format!("failed to read user email: {error}")))?,
        password_hash: row.try_get("password_hash").map_err(|error| {
            ApiError::internal(format!("failed to read user password hash: {error}"))
        })?,
    }))
}

async fn store_token(
    client: &redis::Client,
    access_token: &str,
    stored_value: &str,
) -> Result<(), ApiError> {
    let key = token_key(access_token);
    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .map_err(|error| ApiError::internal(format!("failed to open redis connection: {error}")))?;

    let result: redis::RedisResult<String> = redis::cmd("SETEX")
        .arg(&key)
        .arg(TOKEN_TTL_SECONDS)
        .arg(stored_value)
        .query_async(&mut connection)
        .await;
    result
        .map_err(|error| ApiError::internal(format!("failed to store token in redis: {error}")))?;

    Ok(())
}

async fn store_verification_code(
    client: &redis::Client,
    phone: &str,
    code: &str,
) -> Result<(), ApiError> {
    let key = sms_code_key(phone);
    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .map_err(|error| ApiError::internal(format!("failed to open redis connection: {error}")))?;

    let result: redis::RedisResult<String> = redis::cmd("SETEX")
        .arg(&key)
        .arg(SMS_CODE_TTL_SECONDS)
        .arg(code)
        .query_async(&mut connection)
        .await;
    result.map_err(|error| {
        ApiError::internal(format!(
            "failed to store sms verification code in redis: {error}"
        ))
    })?;

    Ok(())
}

async fn consume_verification_code(
    client: &redis::Client,
    phone: &str,
    expected_code: &str,
) -> Result<(), ApiError> {
    let key = sms_code_key(phone);
    let mut connection = client
        .get_multiplexed_tokio_connection()
        .await
        .map_err(|error| ApiError::internal(format!("failed to open redis connection: {error}")))?;

    let stored_code: Option<String> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut connection)
        .await
        .map_err(|error| {
            ApiError::internal(format!(
                "failed to read sms verification code from redis: {error}"
            ))
        })?;

    let Some(stored_code) = stored_code else {
        return Err(ApiError::unauthorized("invalid or expired sms code"));
    };

    if stored_code != expected_code {
        return Err(ApiError::unauthorized("invalid or expired sms code"));
    }

    let deleted: usize = redis::cmd("DEL")
        .arg(&key)
        .query_async(&mut connection)
        .await
        .map_err(|error| {
            ApiError::internal(format!(
                "failed to delete sms verification code from redis: {error}"
            ))
        })?;

    if deleted == 0 {
        return Err(ApiError::unauthorized("invalid or expired sms code"));
    }

    Ok(())
}

async fn call_sms_provider(
    http_client: &reqwest::Client,
    provider_base_url: &str,
    phone: &str,
    code: &str,
) -> Result<SmsProviderResponse, ApiError> {
    let url = format!("{}/sms/send", provider_base_url.trim_end_matches('/'));
    let response = http_client
        .post(&url)
        .json(&SmsProviderRequest {
            phone: phone.to_string(),
            message: format!("Your verification code is {code}"),
        })
        .send()
        .await
        .map_err(|error| ApiError::bad_gateway(format!("failed to call sms provider: {error}")))?;

    if !response.status().is_success() {
        return Err(ApiError::bad_gateway(format!(
            "sms provider returned HTTP {}",
            response.status()
        )));
    }

    response
        .json::<SmsProviderResponse>()
        .await
        .map_err(|error| {
            ApiError::bad_gateway(format!("failed to parse sms provider response: {error}"))
        })
}

fn resolve_sms_provider_base_url(
    request_provider_base_url: Option<String>,
    state: &AppState,
) -> Result<String, ApiError> {
    if let Some(value) = request_provider_base_url
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
    {
        return Ok(value);
    }

    if let Some(value) = &state.sms_provider_base_url {
        return Ok(value.clone());
    }

    Err(ApiError::service_unavailable(
        "sms provider is not configured; pass provider_base_url or set SMS_PROVIDER_BASE_URL",
    ))
}

fn issue_token(user_id: u64, email: &str, jwt_secret: &str) -> Result<String, ApiError> {
    let issued_at = current_timestamp_seconds()? as usize;
    let claims = JwtClaims {
        sub: user_id.to_string(),
        email: email.to_string(),
        iat: issued_at,
        exp: issued_at + TOKEN_TTL_SECONDS as usize,
    };

    jsonwebtoken::encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(jwt_secret.as_bytes()),
    )
    .map_err(|error| ApiError::internal(format!("failed to encode jwt: {error}")))
}

fn current_timestamp_seconds() -> Result<u64, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| ApiError::internal(format!("failed to read system clock: {error}")))
}

fn current_timestamp_millis() -> Result<u128, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .map_err(|error| ApiError::internal(format!("failed to read system clock: {error}")))
}

fn generate_verification_code() -> Result<String, ApiError> {
    let code = current_timestamp_millis()? % 1_000_000;
    Ok(format!("{code:06}"))
}

fn issue_order_id() -> Result<String, ApiError> {
    Ok(format!("ORD-{}", current_timestamp_millis()?))
}

fn token_key(access_token: &str) -> String {
    format!("{TOKEN_KEY_PREFIX}{access_token}")
}

fn sms_code_key(phone: &str) -> String {
    format!("{SMS_CODE_KEY_PREFIX}{phone}")
}

fn normalize_name(value: String) -> Result<String, ApiError> {
    let name = value.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("name is required"));
    }

    Ok(name.to_string())
}

fn normalize_email(value: String) -> Result<String, ApiError> {
    let email = value.trim().to_ascii_lowercase();
    if email.is_empty() || !email.contains('@') || email.starts_with('@') || email.ends_with('@') {
        return Err(ApiError::bad_request("email must be a valid email address"));
    }

    Ok(email)
}

fn normalize_phone(value: String) -> Result<String, ApiError> {
    let phone = value.trim();
    if phone.len() < 6
        || !phone
            .chars()
            .all(|character| character.is_ascii_digit() || character == '+')
    {
        return Err(ApiError::bad_request(
            "phone must contain at least 6 digits and may only include numbers or a leading +",
        ));
    }

    Ok(phone.to_string())
}

fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let text = value.trim().to_string();
        if text.is_empty() { None } else { Some(text) }
    })
}

fn normalize_coupon_code(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let code = value.trim().to_ascii_uppercase();
        if code.is_empty() { None } else { Some(code) }
    })
}

fn normalize_customer_tier(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let tier = value.trim().to_ascii_lowercase();
        if tier.is_empty() { None } else { Some(tier) }
    })
}

fn normalize_sku(value: String) -> Result<String, ApiError> {
    let sku = value.trim().to_ascii_uppercase();
    if sku.is_empty() {
        return Err(ApiError::bad_request("item sku is required"));
    }
    if !sku.chars().all(|character| {
        character.is_ascii_uppercase() || character.is_ascii_digit() || character == '-'
    }) {
        return Err(ApiError::bad_request(
            "item sku may only contain uppercase letters, digits, or `-`",
        ));
    }

    Ok(sku)
}

fn normalize_sms_code(value: String) -> Result<String, ApiError> {
    let code = value.trim();
    if code.len() != 6 || !code.chars().all(|character| character.is_ascii_digit()) {
        return Err(ApiError::bad_request(
            "sms_code must be a 6-digit verification code",
        ));
    }

    Ok(code.to_string())
}

fn validate_password(password: &str) -> Result<(), ApiError> {
    if password.len() < 8 {
        return Err(ApiError::bad_request(
            "password must be at least 8 characters long",
        ));
    }

    Ok(())
}

fn ensure_password_present(password: &str) -> Result<(), ApiError> {
    if password.is_empty() {
        return Err(ApiError::bad_request("password is required"));
    }

    Ok(())
}

fn normalize_order_customer(
    customer: OrderCustomerRequest,
) -> Result<OrderCustomerResponse, ApiError> {
    Ok(OrderCustomerResponse {
        name: normalize_name(customer.name)?,
        email: normalize_email(customer.email)?,
        tier: normalize_customer_tier(customer.tier),
    })
}

fn normalize_order_items(items: Vec<OrderItemRequest>) -> Result<Vec<OrderItemResponse>, ApiError> {
    if items.is_empty() {
        return Err(ApiError::bad_request("at least one order item is required"));
    }

    items
        .into_iter()
        .map(|item| {
            if item.quantity == 0 {
                return Err(ApiError::bad_request(
                    "item quantity must be greater than 0",
                ));
            }
            if item.unit_price == 0 {
                return Err(ApiError::bad_request(
                    "item unit_price must be greater than 0",
                ));
            }

            let line_total = u64::from(item.quantity)
                .checked_mul(item.unit_price)
                .ok_or_else(|| ApiError::internal("failed to calculate item line_total"))?;

            Ok(OrderItemResponse {
                sku: normalize_sku(item.sku)?,
                quantity: item.quantity,
                unit_price: item.unit_price,
                line_total,
            })
        })
        .collect()
}

fn build_order_pricing(
    items: &[OrderItemResponse],
    coupon_code: Option<&str>,
    tier: Option<&str>,
) -> Result<OrderPricingResponse, ApiError> {
    let sku_count = items.len() as u64;
    let item_count = items.iter().try_fold(0u64, |count, item| {
        count
            .checked_add(u64::from(item.quantity))
            .ok_or_else(|| ApiError::internal("failed to calculate item_count"))
    })?;
    let subtotal = items.iter().try_fold(0u64, |total, item| {
        total
            .checked_add(item.line_total)
            .ok_or_else(|| ApiError::internal("failed to calculate subtotal"))
    })?;
    let discount = compute_order_discount(subtotal, coupon_code, tier)?;
    let discounted_subtotal = subtotal
        .checked_sub(discount)
        .ok_or_else(|| ApiError::internal("discount cannot exceed subtotal"))?;
    let shipping_fee = if item_count >= 3 || discounted_subtotal >= 10_000 {
        0
    } else {
        1_200
    };
    let payable_total = discounted_subtotal
        .checked_add(shipping_fee)
        .ok_or_else(|| ApiError::internal("failed to calculate payable_total"))?;

    Ok(OrderPricingResponse {
        sku_count,
        item_count,
        subtotal,
        discount,
        shipping_fee,
        payable_total,
    })
}

fn compute_order_discount(
    subtotal: u64,
    coupon_code: Option<&str>,
    tier: Option<&str>,
) -> Result<u64, ApiError> {
    match coupon_code {
        None => Ok(0),
        Some("SAVE10") => Ok(subtotal / 10),
        Some("VIP50") if tier == Some("vip") => Ok(subtotal.min(5_000)),
        Some("VIP50") => Err(ApiError::bad_request(
            "coupon_code `VIP50` requires customer tier `vip`",
        )),
        Some(other) => Err(ApiError::bad_request(format!(
            "coupon_code `{other}` is not supported"
        ))),
    }
}

fn hash_password(password: &str) -> Result<String, ApiError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|_| ApiError::internal("failed to hash password"))
}

fn verify_password(password: &str, password_hash: &str) -> Result<bool, ApiError> {
    let parsed_hash = PasswordHash::new(password_hash).map_err(|error| {
        ApiError::internal(format!("failed to parse stored password hash: {error}"))
    })?;

    match Argon2::default().verify_password(password.as_bytes(), &parsed_hash) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::Password) => Ok(false),
        Err(error) => Err(ApiError::internal(format!(
            "failed to verify password hash: {error}"
        ))),
    }
}

fn map_database_error(error: sqlx::Error) -> ApiError {
    if let sqlx::Error::Database(database_error) = &error
        && database_error.code().as_deref() == Some("1062")
    {
        return ApiError::conflict("email is already registered");
    }

    ApiError::internal(format!("database error: {error}"))
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, message)
    }

    fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, message)
    }

    fn bad_gateway(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, message)
    }

    fn service_unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, message)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, message)
    }

    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ErrorResponse {
                error: self.message,
            }),
        )
            .into_response()
    }
}
