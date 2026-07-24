//! Server-rendered Open Graph / Twitter social-card HTML for link unfurls (X, Telegram,
//! Discord, Slack, …). Crawlers don't run the SPA's JavaScript, so these `<meta>` tags have to
//! be in the HTML the server returns — the client-set `document.title` never reaches them.
//!
//! nginx routes *only* crawler User-Agents to the axum fallback (`og_page` in `server.rs`);
//! humans keep getting the static SPA shell. This module is the pure card model + HTML renderer
//! + formatting helpers (all unit-tested); `og_page` gathers the per-page data.

/// A resolved social card. `image` / `image_twitter` are absolute URLs.
pub struct Card {
    pub title: String,
    pub description: String,
    pub image: String,
    pub image_twitter: String,
    /// `summary_large_image` (a big banner, for the NFT image) vs `summary` (the logo).
    pub large: bool,
    /// Optional SEO overrides for `<title>` / `<meta name="description">` (the search snippet),
    /// independent of the social `og:` / `twitter:` title & description. `None` derives them from
    /// `title` / `description` — set them (e.g. on the home page) to show a tagline to search
    /// engines while the social card keeps its own text.
    pub seo_title: Option<String>,
    pub seo_description: Option<String>,
}

impl Card {
    /// A logo-backed `summary` card, for pages with no natural image (pool/addr/stake/drep/…).
    pub fn branded(base_url: &str, title: String, description: String) -> Self {
        Card {
            title,
            description,
            image: format!("{base_url}/logo.jpg"),
            image_twitter: format!("{base_url}/logo_square.jpg"),
            large: false,
            seo_title: None,
            seo_description: None,
        }
    }

    /// A `summary_large_image` card with a specific image (the NFT `/image` @1024).
    pub fn with_image(title: String, description: String, image: String) -> Self {
        Card {
            title,
            description,
            image_twitter: image.clone(),
            image,
            large: true,
            seo_title: None,
            seo_description: None,
        }
    }
}

/// The full HTML document a crawler receives. Serves double duty: the `og:` / `twitter:` head for
/// social unfurls, and — since nginx also routes search-engine bots here — SEO essentials (a
/// descriptive `<title>`, `<meta name="description">`, `<link rel="canonical">`, `robots`) plus a
/// small crawlable body (`<h1>` + the facts + a link) so Bing/Brave, which barely run JS, index
/// real content instead of the bare `pool.pm` shell. Humans never reach this route.
pub fn render(card: &Card, url: &str) -> String {
    let twitter_card = if card.large {
        "summary_large_image"
    } else {
        "summary"
    };
    // Social title/description (og:/twitter:), and the SEO title/description (search snippet),
    // which default to the social ones unless the card overrides them (e.g. the home tagline).
    let title = esc(&card.title);
    let desc = esc(&card.description);
    let seo_title = esc(&card
        .seo_title
        .clone()
        .unwrap_or_else(|| format!("{} · pool.pm", card.title)));
    let seo_desc = esc(card.seo_description.as_deref().unwrap_or(&card.description));
    // The description carries `\n` line breaks for the cards; flatten them for the body prose.
    let body_desc = esc(&card.description.replace('\n', " · "));
    let url = esc(url);
    let image = esc(&card.image);
    let twimage = esc(&card.image_twitter);
    format!(
        "<!doctype html>\n<html lang=\"en\">\n<head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{seo_title}</title>\n\
         <link rel=\"canonical\" href=\"{url}\">\n\
         <meta name=\"robots\" content=\"index, follow\">\n\
         <meta name=\"description\" content=\"{seo_desc}\">\n\
         <meta property=\"og:site_name\" content=\"pool.pm\">\n\
         <meta property=\"og:type\" content=\"website\">\n\
         <meta property=\"og:url\" content=\"{url}\">\n\
         <meta property=\"og:title\" content=\"{title}\">\n\
         <meta property=\"og:description\" content=\"{desc}\">\n\
         <meta property=\"og:image\" content=\"{image}\">\n\
         <meta name=\"twitter:card\" content=\"{twitter_card}\">\n\
         <meta name=\"twitter:site\" content=\"@pool_pm\">\n\
         <meta name=\"twitter:title\" content=\"{title}\">\n\
         <meta name=\"twitter:description\" content=\"{desc}\">\n\
         <meta name=\"twitter:image\" content=\"{twimage}\">\n\
         </head>\n<body>\n\
         <h1>{title}</h1>\n\
         <p>{body_desc}</p>\n\
         <p><a href=\"{url}\">Open on pool.pm</a></p>\n\
         </body>\n</html>\n"
    )
}

/// Escape a string for an HTML double-quoted attribute value.
pub fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// A lovelace amount as whole ADA with thousands separators, e.g. `1_234_567_000000` → `"₳1,234,567"`.
pub fn fmt_ada(lovelace: i64) -> String {
    format!("₳{}", commas(lovelace / 1_000_000))
}

/// An integer with thousands separators, e.g. `3179` → `"3,179"`.
pub fn commas(n: i64) -> String {
    let neg = n < 0;
    let digits = n.unsigned_abs().to_string();
    let bytes = digits.as_bytes();
    let mut out = String::with_capacity(bytes.len() + bytes.len() / 3 + 1);
    if neg {
        out.push('-');
    }
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

/// Pool ticker for display: up to 5 uppercased alphanumerics — from the ticker if present, else
/// the pool id after `pool1`. Mirrors the frontend `formatTicker`.
pub fn format_ticker(s: &str) -> String {
    s.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .take(5)
        .collect()
}

/// Short id for display: first 8 … last 4 (policy/drep fallback). Inputs here are ASCII
/// (bech32 / hex), so byte slicing is safe.
pub fn short_id(s: &str) -> String {
    if s.len() > 16 {
        format!("{}…{}", &s[..8], &s[s.len() - 4..])
    } else {
        s.to_string()
    }
}

/// Join non-empty parts with " · " — the card description separator.
pub fn join(parts: &[String]) -> String {
    parts
        .iter()
        .filter(|p| !p.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_ada_groups_and_truncates_to_whole_ada() {
        assert_eq!(fmt_ada(1_234_567_000_000), "₳1,234,567");
        assert_eq!(fmt_ada(999_999), "₳0"); // < 1 ADA
        assert_eq!(fmt_ada(0), "₳0");
        assert_eq!(fmt_ada(45_000_000), "₳45");
    }

    #[test]
    fn format_ticker_uppercases_and_caps_at_five() {
        assert_eq!(format_ticker("smaug"), "SMAUG");
        assert_eq!(format_ticker("a-b.c d e f"), "ABCDE"); // strips non-alnum, caps 5
                                                           // From a pool id tail (pool_id.slice(5,10) equivalent input):
        assert_eq!(format_ticker("qx7yz"), "QX7YZ");
    }

    #[test]
    fn esc_escapes_attribute_breakers() {
        assert_eq!(
            esc(r#"a & b < c > "d""#),
            "a &amp; b &lt; c &gt; &quot;d&quot;"
        );
    }

    #[test]
    fn render_picks_card_type_and_escapes() {
        let card = Card::with_image(
            r#"Cool "NFT""#.to_string(),
            "desc".to_string(),
            "https://x/image?size=1024".to_string(),
        );
        let html = render(&card, "https://pool.pm/asset1abc");
        assert!(html.contains(r#"twitter:card" content="summary_large_image""#));
        assert!(html.contains(r#"og:title" content="Cool &quot;NFT&quot;""#));
        assert!(html.contains(r#"og:image" content="https://x/image?size=1024""#));

        let branded = Card::branded("https://pool.pm", "pool.pm".to_string(), "d".to_string());
        let html = render(&branded, "https://pool.pm/");
        assert!(html.contains(r#"twitter:card" content="summary""#));
        assert!(html.contains(r#"og:image" content="https://pool.pm/logo.jpg""#));
        assert!(html.contains(r#"twitter:image" content="https://pool.pm/logo_square.jpg""#));
    }

    #[test]
    fn join_drops_empty_parts() {
        assert_eq!(
            join(&["₳100".into(), "".into(), "SMAUG".into()]),
            "₳100 · SMAUG"
        );
    }
}
