#![forbid(unsafe_code)]
#![allow(clippy::expect_used)]
#![allow(clippy::unwrap_used)]
//! Living example: register → login → mint JWT.
//!
//! ```sh
//! cargo run --example auth_register_login
//! ```

use tokenkit::service::{JwtAlgorithm, JwtConfig, JwtService};

fn main() {
    // Registration boundary: validkit rejects the bad address pre-hash.
    assert!(validkit::EmailAddr::parse("not-an-email").is_err());
    let email = validkit::EmailAddr::parse("wyatt@example.com").expect("valid");
    println!("registered: {email}");

    // Password stored as an Argon2id PHC hash with a server-side pepper.
    let pepper = salting::Pepper::new(b"example-pepper").expect("pepper");
    let hash = salting::hash_password_with_pepper("s3cret-pass!", &pepper).expect("hash");
    println!("stored hash: {}", &hash[..20]);

    // Login: verify, then mint a JWT for the authenticated session.
    let ok = salting::verify_password_with_pepper("s3cret-pass!", &hash, &pepper).expect("verify");
    assert!(ok);
    let service = JwtService::new(JwtConfig {
        issuer: Some("estate-integration".into()),
        ..JwtConfig::with_rotation_secrets(JwtAlgorithm::HS256, vec!["example-secret".into()])
    });

    #[derive(serde::Serialize, serde::Deserialize, Debug)]
    struct Session {
        sub: String,
        email: String,
    }
    let token = service
        .encode(&Session {
            sub: "user-1".into(),
            email: email.as_str().into(),
        })
        .expect("mint");
    let back: Session = service.decode(&token).expect("decode");
    println!("authenticated session for {} ({})", back.sub, back.email);
}
