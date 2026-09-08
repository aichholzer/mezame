//! Passwords, the session cookie and the login limiter (Requirement 5 of
//! the phase 2 spec), as pure functions.

use std::time::{Duration, Instant};

use mezame::auth::{
    check_password, clear_cookie_header, cookie_value, dummy_hash, hash_password,
    set_cookie_header, sign, verify, verify_calls_for_test, verify_password, AuthError, Cookie,
    RateLimiter, COOKIE_LIFETIME, COOKIE_NAME, COOKIE_RENEW_WITHIN, LOGIN_LIMIT, LOGIN_WINDOW,
    MAX_PASSWORD_BYTES, MIN_PASSWORD_CHARS,
};
use mezame::store::crypto::{MasterKey, KEY_LEN};

const USER: &str = "0123456789abcdef0123456789abcdef";

fn key() -> [u8; KEY_LEN] {
    MasterKey::from_bytes_for_test([7u8; KEY_LEN]).keys().cookie
}

#[test]
fn a_password_hashes_to_a_phc_string_and_verifies_only_itself() {
    let hash = hash_password("correct horse battery").unwrap();
    assert!(
        hash.starts_with("$argon2id$v=19$m=19456,t=2,p=1$"),
        "{hash}"
    );
    assert!(verify_password("correct horse battery", &hash));
    assert!(!verify_password("correct horse batter", &hash));
    assert!(!verify_password("", &hash));
    // Two hashes of one password differ by their salt and both verify.
    let again = hash_password("correct horse battery").unwrap();
    assert_ne!(hash, again);
    assert!(verify_password("correct horse battery", &again));
    // A stored string that is not a PHC hash verifies nothing.
    assert!(!verify_password("correct horse battery", "not a hash"));
    assert!(!verify_password("correct horse battery", ""));
    // Every call counts, including the failures.
    let before = verify_calls_for_test();
    verify_password("x", &hash);
    verify_password("x", "junk");
    assert_eq!(verify_calls_for_test() - before, 2);
}

#[test]
fn the_length_rules_are_named() {
    assert_eq!(check_password("short"), Err(AuthError::TooShort));
    assert_eq!(hash_password("1234567").unwrap_err(), AuthError::TooShort);
    assert!(
        check_password("12345678").is_ok(),
        "eight characters is enough"
    );
    assert!(check_password("pass word with spaces").is_ok());
    assert!(
        check_password("ぱすわーどですよ").is_ok(),
        "eight characters, not bytes"
    );
    let long = "x".repeat(MAX_PASSWORD_BYTES + 1);
    assert_eq!(check_password(&long), Err(AuthError::TooLong));
    assert!(check_password(&"x".repeat(MAX_PASSWORD_BYTES)).is_ok());
    assert!(AuthError::TooShort
        .to_string()
        .contains(&MIN_PASSWORD_CHARS.to_string()));
    assert!(AuthError::TooLong
        .to_string()
        .contains(&MAX_PASSWORD_BYTES.to_string()));
}

#[test]
fn the_dummy_hash_is_a_real_hash_nobody_matches() {
    let dummy = dummy_hash();
    assert!(dummy.starts_with("$argon2id$"), "{dummy}");
    assert_eq!(dummy, dummy_hash(), "made once");
    assert!(!verify_password("", dummy));
    assert!(!verify_password("password", dummy));
}

#[test]
fn a_cookie_signs_and_verifies_and_every_tamper_fails() {
    let key = key();
    let now = 1_700_000_000;
    let cookie = Cookie::issue(USER, 3, now);
    assert_eq!(cookie.expiry, now + COOKIE_LIFETIME.as_secs() as i64);
    let value = sign(&cookie, &key);
    let parts: Vec<&str> = value.split('.').collect();
    assert_eq!(parts.len(), 4);
    assert_eq!(parts[0], USER);
    assert_eq!(parts[2], "3");
    assert_eq!(
        parts[3].len(),
        43,
        "a 32-byte MAC in base64url without padding"
    );
    assert_eq!(verify(&value, &key, now), Some(cookie.clone()));
    assert_eq!(
        verify(&value, &key, cookie.expiry - 1),
        Some(cookie.clone())
    );
    // Expiry at the boundary and after.
    assert_eq!(verify(&value, &key, cookie.expiry), None);
    assert_eq!(verify(&value, &key, cookie.expiry + 1), None);
    // Another key.
    let other = MasterKey::from_bytes_for_test([8u8; KEY_LEN]).keys().cookie;
    assert_eq!(verify(&value, &other, now), None);
    // A tampered user id, expiry, epoch and MAC.
    let tampered_id = value.replacen(USER, "fedcba9876543210fedcba9876543210", 1);
    assert_eq!(verify(&tampered_id, &key, now), None);
    let tampered_expiry = value.replacen(
        &cookie.expiry.to_string(),
        &(cookie.expiry + 1).to_string(),
        1,
    );
    assert_eq!(verify(&tampered_expiry, &key, now), None);
    let tampered_epoch = format!("{}.{}.{}.{}", parts[0], parts[1], "4", parts[3]);
    assert_eq!(verify(&tampered_epoch, &key, now), None);
    let mut mac = parts[3].to_string();
    let first = mac.remove(0);
    mac.insert(0, if first == 'A' { 'B' } else { 'A' });
    let tampered_mac = format!("{}.{}.{}.{mac}", parts[0], parts[1], parts[2]);
    assert_eq!(verify(&tampered_mac, &key, now), None);
    // A truncated or padded MAC, and a padding character.
    assert_eq!(verify(&value[..value.len() - 1], &key, now), None);
    assert_eq!(verify(&format!("{value}A"), &key, now), None);
    assert_eq!(verify(&format!("{value}="), &key, now), None);
    // Wrong shapes.
    for bad in [
        "",
        "a.b.c",
        "a.b.c.d.e",
        "nothex.1.2.AAAA",
        &format!("{USER}.x.3.AAAA"),
    ] {
        assert_eq!(verify(bad, &key, now), None, "{bad}");
    }
}

#[test]
fn the_last_mac_character_verifies_in_one_spelling_only() {
    // The 43rd base64url character carries two data bits and four slack
    // bits; a lenient decoder would read four spellings as the same tag.
    let key = key();
    let now = 1_700_000_000;
    let value = sign(&Cookie::issue(USER, 0, now), &key);
    let alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let last = value.chars().last().unwrap();
    let index = alphabet.find(last).unwrap();
    let mut verified = 0;
    for (i, candidate) in alphabet.chars().enumerate() {
        let mut altered = value.clone();
        altered.pop();
        altered.push(candidate);
        if verify(&altered, &key, now).is_some() {
            verified += 1;
            assert_eq!(i, index, "only the genuine spelling verifies");
        }
    }
    assert_eq!(verified, 1);
}

#[test]
fn renewal_is_due_under_thirty_days() {
    let now = 1_700_000_000;
    let cookie = Cookie::issue(USER, 0, now);
    let day = 24 * 60 * 60;
    let renew = COOKIE_RENEW_WITHIN.as_secs() as i64;
    let lifetime = COOKIE_LIFETIME.as_secs() as i64;
    assert!(!cookie.due_for_renewal(now));
    assert!(
        !cookie.due_for_renewal(now + lifetime - renew - day),
        "31 days left"
    );
    assert!(
        cookie.due_for_renewal(now + lifetime - renew + day),
        "29 days left"
    );
}

#[test]
fn the_cookie_headers_carry_the_attributes() {
    let header = set_cookie_header("v", false);
    assert_eq!(
        header,
        format!(
            "{COOKIE_NAME}=v; Path=/; HttpOnly; SameSite=Lax; Max-Age={}",
            COOKIE_LIFETIME.as_secs()
        )
    );
    assert!(set_cookie_header("v", true).ends_with("; Secure"));
    assert!(!header.contains("Secure"));
    assert_eq!(
        clear_cookie_header(),
        format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
    );
    assert_eq!(cookie_value("a=1; mezame_session=xyz; b=2"), Some("xyz"));
    assert_eq!(cookie_value("mezame_session=xyz"), Some("xyz"));
    assert_eq!(cookie_value("other=1"), None);
    assert_eq!(cookie_value(""), None);
    assert_eq!(cookie_value("mezame_sessionx=1"), None);
}

#[test]
fn the_limiter_admits_ten_then_refuses_until_the_window_turns() {
    let limiter = RateLimiter::default();
    let start = Instant::now();
    for i in 0..LOGIN_LIMIT {
        assert!(limiter.check("u:alice", start).is_ok(), "attempt {i}");
    }
    let left = limiter
        .check("u:alice", start + Duration::from_secs(10))
        .unwrap_err();
    assert!(left <= LOGIN_WINDOW - Duration::from_secs(10), "{left:?}");
    assert!(left >= Duration::from_secs(1));
    // Another name is another bucket.
    assert!(limiter.check("u:bob", start).is_ok());
    // The window turns and the count starts over.
    assert!(limiter.check("u:alice", start + LOGIN_WINDOW).is_ok());
    assert_eq!(limiter.len(), 2);
}

#[test]
fn the_limiter_evicts_its_oldest_window_at_the_cap() {
    let limiter = RateLimiter::new(10, LOGIN_WINDOW, 3);
    let start = Instant::now();
    limiter.check("u:a", start).unwrap();
    limiter
        .check("u:b", start + Duration::from_secs(1))
        .unwrap();
    limiter
        .check("u:c", start + Duration::from_secs(2))
        .unwrap();
    assert_eq!(limiter.len(), 3);
    limiter
        .check("u:d", start + Duration::from_secs(3))
        .unwrap();
    assert_eq!(limiter.len(), 3, "the cap holds");
    // `a` was evicted: its count starts over, so ten more pass.
    for _ in 0..10 {
        limiter
            .check("u:a", start + Duration::from_secs(4))
            .unwrap();
    }
    assert!(limiter
        .check("u:a", start + Duration::from_secs(4))
        .is_err());
    assert!(!limiter.is_empty());
}
