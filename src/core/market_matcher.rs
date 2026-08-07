//! Matches similar markets between Polymarket and Kalshi using text
//! similarity, keyword matching, and sports-specific logic (mirrors the
//! `MarketMatcher` class in `core/cross_platform_arb.py`).

use crate::kalshi_models::KalshiMarket;
use crate::models::Market;
use chrono::{DateTime, Utc};
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashMap, HashSet};
use tracing::info;

#[derive(Debug, Clone)]
pub struct MarketPair {
    pub polymarket_id: String,
    pub kalshi_ticker: String,
    pub polymarket_question: String,
    pub kalshi_title: String,
    pub similarity_score: f64,
    pub category: String,
    pub matched_at: DateTime<Utc>,
}

impl MarketPair {
    pub fn pair_id(&self) -> String {
        format!("poly:{}|kalshi:{}", self.polymarket_id, self.kalshi_ticker)
    }
}

const NOISE_WORDS: &[&str] = &[
    "will", "the", "a", "an", "be", "to", "in", "on", "by", "at", "what", "who", "which", "when", "is", "are", "was", "were", "market", "prediction", "bet", "odds", "win", "winner",
];

static NFL_TEAMS: Lazy<HashMap<&'static str, Vec<&'static str>>> = Lazy::new(|| {
    HashMap::from([
        ("arizona cardinals", vec!["cardinals", "arizona", "ari"]),
        ("atlanta falcons", vec!["falcons", "atlanta", "atl"]),
        ("baltimore ravens", vec!["ravens", "baltimore", "bal"]),
        ("buffalo bills", vec!["bills", "buffalo", "buf"]),
        ("carolina panthers", vec!["panthers", "carolina", "car"]),
        ("chicago bears", vec!["bears", "chicago", "chi"]),
        ("cincinnati bengals", vec!["bengals", "cincinnati", "cin"]),
        ("cleveland browns", vec!["browns", "cleveland", "cle"]),
        ("dallas cowboys", vec!["cowboys", "dallas", "dal"]),
        ("denver broncos", vec!["broncos", "denver", "den"]),
        ("detroit lions", vec!["lions", "detroit", "det"]),
        ("green bay packers", vec!["packers", "green bay", "gb"]),
        ("houston texans", vec!["texans", "houston", "hou"]),
        ("indianapolis colts", vec!["colts", "indianapolis", "ind"]),
        ("jacksonville jaguars", vec!["jaguars", "jacksonville", "jax"]),
        ("kansas city chiefs", vec!["chiefs", "kansas city", "kc"]),
        ("las vegas raiders", vec!["raiders", "las vegas", "lv"]),
        ("los angeles chargers", vec!["chargers", "la chargers", "lac"]),
        ("los angeles rams", vec!["rams", "la rams", "lar"]),
        ("miami dolphins", vec!["dolphins", "miami", "mia"]),
        ("minnesota vikings", vec!["vikings", "minnesota", "min"]),
        ("new england patriots", vec!["patriots", "new england", "ne"]),
        ("new orleans saints", vec!["saints", "new orleans", "no"]),
        ("new york giants", vec!["giants", "ny giants", "nyg"]),
        ("new york jets", vec!["jets", "ny jets", "nyj"]),
        ("philadelphia eagles", vec!["eagles", "philadelphia", "phi"]),
        ("pittsburgh steelers", vec!["steelers", "pittsburgh", "pit"]),
        ("san francisco 49ers", vec!["49ers", "san francisco", "sf"]),
        ("seattle seahawks", vec!["seahawks", "seattle", "sea"]),
        ("tampa bay buccaneers", vec!["buccaneers", "tampa bay", "tb"]),
        ("tennessee titans", vec!["titans", "tennessee", "ten"]),
        ("washington commanders", vec!["commanders", "washington", "was"]),
    ])
});

static NBA_TEAMS: Lazy<HashMap<&'static str, Vec<&'static str>>> = Lazy::new(|| {
    HashMap::from([
        ("boston celtics", vec!["celtics", "boston"]),
        ("brooklyn nets", vec!["nets", "brooklyn"]),
        ("new york knicks", vec!["knicks", "new york"]),
        ("philadelphia 76ers", vec!["76ers", "sixers", "philadelphia"]),
        ("toronto raptors", vec!["raptors", "toronto"]),
        ("chicago bulls", vec!["bulls", "chicago"]),
        ("cleveland cavaliers", vec!["cavaliers", "cavs", "cleveland"]),
        ("detroit pistons", vec!["pistons", "detroit"]),
        ("indiana pacers", vec!["pacers", "indiana"]),
        ("milwaukee bucks", vec!["bucks", "milwaukee"]),
        ("atlanta hawks", vec!["hawks", "atlanta"]),
        ("charlotte hornets", vec!["hornets", "charlotte"]),
        ("miami heat", vec!["heat", "miami"]),
        ("orlando magic", vec!["magic", "orlando"]),
        ("washington wizards", vec!["wizards", "washington"]),
        ("denver nuggets", vec!["nuggets", "denver"]),
        ("minnesota timberwolves", vec!["timberwolves", "wolves", "minnesota"]),
        ("oklahoma city thunder", vec!["thunder", "okc"]),
        ("portland trail blazers", vec!["blazers", "portland"]),
        ("utah jazz", vec!["jazz", "utah"]),
        ("golden state warriors", vec!["warriors", "golden state"]),
        ("los angeles clippers", vec!["clippers", "la clippers"]),
        ("los angeles lakers", vec!["lakers", "la lakers"]),
        ("phoenix suns", vec!["suns", "phoenix"]),
        ("sacramento kings", vec!["kings", "sacramento"]),
        ("dallas mavericks", vec!["mavericks", "mavs", "dallas"]),
        ("houston rockets", vec!["rockets", "houston"]),
        ("memphis grizzlies", vec!["grizzlies", "memphis"]),
        ("new orleans pelicans", vec!["pelicans", "new orleans"]),
        ("san antonio spurs", vec!["spurs", "san antonio"]),
    ])
});

/// Reverse lookup: every team name/variant (lowercase) -> canonical full name.
static TEAM_LOOKUP: Lazy<HashMap<String, String>> = Lazy::new(|| {
    let mut lookup = HashMap::new();
    for (&full_name, variants) in NFL_TEAMS.iter().chain(NBA_TEAMS.iter()) {
        lookup.insert(full_name.to_string(), full_name.to_string());
        for variant in variants {
            lookup.insert(variant.to_lowercase(), full_name.to_string());
        }
    }
    lookup
});

static NON_WORD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"[^\w\s]").unwrap());
static NUMBER_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\d+(?:\.\d+)?%?").unwrap());
static CAPITALIZED_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\b[A-Z][a-z]+(?:\s+[A-Z][a-z]+)*\b").unwrap());
static SLASH_DATE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(\d{1,2})[/-](\d{1,2})[/-](\d{2,4})").unwrap());

static MONTH_RES: Lazy<Vec<(&'static str, Regex)>> = Lazy::new(|| {
    let months: &[(&str, &str)] = &[
        ("jan", "01"), ("january", "01"), ("feb", "02"), ("february", "02"),
        ("mar", "03"), ("march", "03"), ("apr", "04"), ("april", "04"),
        ("may", "05"), ("jun", "06"), ("june", "06"), ("jul", "07"), ("july", "07"),
        ("aug", "08"), ("august", "08"), ("sep", "09"), ("september", "09"),
        ("oct", "10"), ("october", "10"), ("nov", "11"), ("november", "11"),
        ("dec", "12"), ("december", "12"),
    ];
    months
        .iter()
        .map(|(name, num)| (*num, Regex::new(&format!(r"{name}\.?\s+(\d{{1,2}})(?:,?\s+(\d{{4}}))?")).unwrap()))
        .collect()
});

const POLITICAL_TERMS: &[&str] = &["trump", "biden", "republican", "democrat", "gop", "dnc", "harris", "desantis", "election", "president"];
const CRYPTO_TERMS: &[&str] = &["bitcoin", "btc", "ethereum", "eth", "crypto", "solana", "sol"];
const PERSON_NAMES: &[&str] = &["trump", "biden", "harris", "desantis", "obama", "pence", "musk", "zuckerberg", "bezos", "gates", "powell", "yellen"];
const ACTION_WORDS: &[&str] = &["win", "lose", "approve", "poll", "elect", "resign", "indicted", "indict", "convicted", "convict"];

pub struct MarketMatcher {
    pub min_similarity: f64,
    matched_pairs: HashMap<String, MarketPair>,
}

impl MarketMatcher {
    pub fn new(min_similarity: f64) -> Self {
        Self { min_similarity, matched_pairs: HashMap::new() }
    }

    pub fn normalize_text(&self, text: &str) -> String {
        let lower = text.to_lowercase();
        let cleaned = NON_WORD_RE.replace_all(&lower, " ");
        cleaned.split_whitespace().filter(|w| !NOISE_WORDS.contains(w)).collect::<Vec<_>>().join(" ")
    }

    pub fn extract_teams(&self, text: &str) -> Vec<String> {
        let mut text_lower = text.to_lowercase();
        let mut found = Vec::new();

        let mut keys: Vec<&String> = TEAM_LOOKUP.keys().collect();
        keys.sort_by_key(|k| std::cmp::Reverse(k.len()));

        for key in keys {
            if text_lower.contains(key.as_str()) {
                let canonical = TEAM_LOOKUP.get(key).unwrap();
                if !found.contains(canonical) {
                    found.push(canonical.clone());
                    text_lower = text_lower.replace(key.as_str(), "");
                }
            }
        }

        found
    }

    pub fn extract_key_entities(&self, text: &str) -> HashSet<String> {
        let mut entities = HashSet::new();

        for m in NUMBER_RE.find_iter(text) {
            entities.insert(m.as_str().to_string());
        }
        for m in CAPITALIZED_RE.find_iter(text) {
            entities.insert(m.as_str().to_string());
        }

        let lower = text.to_lowercase();
        for term in POLITICAL_TERMS {
            if lower.contains(term) {
                entities.insert(term.to_string());
            }
        }
        for term in CRYPTO_TERMS {
            if lower.contains(term) {
                entities.insert(term.to_string());
            }
        }

        entities
    }

    pub fn extract_date(&self, text: &str) -> Option<String> {
        let lower = text.to_lowercase();

        for (month_num, re) in MONTH_RES.iter() {
            if let Some(caps) = re.captures(&lower) {
                let day: u32 = caps.get(1)?.as_str().parse().ok()?;
                let year = caps.get(2).map(|m| m.as_str().to_string()).unwrap_or_else(|| "2024".to_string());
                return Some(format!("{year}-{month_num}-{day:02}"));
            }
        }

        if let Some(caps) = SLASH_DATE_RE.captures(text) {
            let month: u32 = caps[1].parse().ok()?;
            let day: u32 = caps[2].parse().ok()?;
            let mut year = caps[3].to_string();
            if year.len() == 2 {
                year = format!("20{year}");
            }
            return Some(format!("{year}-{month:02}-{day:02}"));
        }

        None
    }

    pub fn dates_match(&self, date1: Option<&str>, date2: Option<&str>) -> bool {
        match (date1, date2) {
            (Some(d1), Some(d2)) => d1 == d2,
            _ => true, // If no dates, don't penalize.
        }
    }

    /// Check if two texts refer to the same sports matchup. Returns
    /// `(is_match, confidence_score)`.
    pub fn is_sports_match(&self, text1: &str, text2: &str) -> (bool, f64) {
        let teams1 = self.extract_teams(text1);
        let teams2 = self.extract_teams(text2);

        if teams1.len() >= 2 && teams2.len() >= 2 {
            let teams1_set: HashSet<&String> = teams1[..2].iter().collect();
            let teams2_set: HashSet<&String> = teams2[..2].iter().collect();

            if teams1_set == teams2_set {
                let date1 = self.extract_date(text1);
                let date2 = self.extract_date(text2);
                return if self.dates_match(date1.as_deref(), date2.as_deref()) { (true, 0.95) } else { (false, 0.3) };
            }

            let overlap = teams1_set.intersection(&teams2_set).count();
            if overlap >= 1 {
                let date1 = self.extract_date(text1);
                let date2 = self.extract_date(text2);
                if self.dates_match(date1.as_deref(), date2.as_deref()) {
                    return (true, 0.7 + (0.2 * overlap as f64 / 2.0));
                }
            }
        }

        (false, 0.0)
    }

    /// Check if two texts refer to the same person-related prediction.
    pub fn is_same_person_event(&self, text1: &str, text2: &str) -> (bool, f64) {
        let lower1 = text1.to_lowercase();
        let lower2 = text2.to_lowercase();

        let persons1: HashSet<&str> = PERSON_NAMES.iter().filter(|p| lower1.contains(**p)).copied().collect();
        let persons2: HashSet<&str> = PERSON_NAMES.iter().filter(|p| lower2.contains(**p)).copied().collect();

        if !persons1.is_empty() && !persons2.is_empty() && persons1.intersection(&persons2).next().is_some() {
            let actions1: HashSet<&str> = ACTION_WORDS.iter().filter(|a| lower1.contains(**a)).copied().collect();
            let actions2: HashSet<&str> = ACTION_WORDS.iter().filter(|a| lower2.contains(**a)).copied().collect();

            return if actions1.intersection(&actions2).next().is_some() { (true, 0.85) } else { (true, 0.6) };
        }

        (false, 0.0)
    }

    /// Calculate similarity score (0-1) between two market questions using
    /// sports/person matching first, then fuzzy text + entity overlap.
    pub fn calculate_similarity(&self, polymarket_question: &str, kalshi_title: &str) -> f64 {
        let (is_sports, sports_score) = self.is_sports_match(polymarket_question, kalshi_title);
        if is_sports && sports_score > 0.7 {
            return sports_score;
        }

        let (is_person, person_score) = self.is_same_person_event(polymarket_question, kalshi_title);
        if is_person && person_score > 0.7 {
            return person_score;
        }

        let norm_poly = self.normalize_text(polymarket_question);
        let norm_kalshi = self.normalize_text(kalshi_title);

        let text_sim = crate::core::sequence_ratio::ratio(&norm_poly, &norm_kalshi);

        let poly_entities = self.extract_key_entities(polymarket_question);
        let kalshi_entities = self.extract_key_entities(kalshi_title);

        let mut combined_sim = if !poly_entities.is_empty() && !kalshi_entities.is_empty() {
            let overlap = poly_entities.intersection(&kalshi_entities).count();
            let entity_overlap = overlap as f64 / poly_entities.len().max(kalshi_entities.len()) as f64;
            0.5 * text_sim + 0.5 * entity_overlap
        } else {
            text_sim
        };

        let sport_keywords = ["nfl", "nba", "mlb", "nhl", "football", "basketball", "baseball", "hockey"];
        let poly_lower = polymarket_question.to_lowercase();
        let kalshi_lower = kalshi_title.to_lowercase();
        let poly_sports: HashSet<&str> = sport_keywords.iter().filter(|s| poly_lower.contains(**s)).copied().collect();
        let kalshi_sports: HashSet<&str> = sport_keywords.iter().filter(|s| kalshi_lower.contains(**s)).copied().collect();
        if !poly_sports.is_empty() && !kalshi_sports.is_empty() && poly_sports.intersection(&kalshi_sports).next().is_some() {
            combined_sim = (combined_sim + 0.15).min(1.0);
        }

        let crypto_keywords = ["bitcoin", "btc", "ethereum", "eth", "solana", "sol"];
        let poly_crypto: HashSet<&str> = crypto_keywords.iter().filter(|c| poly_lower.contains(**c)).copied().collect();
        let kalshi_crypto: HashSet<&str> = crypto_keywords.iter().filter(|c| kalshi_lower.contains(**c)).copied().collect();
        if !poly_crypto.is_empty() && !kalshi_crypto.is_empty() && poly_crypto.intersection(&kalshi_crypto).next().is_some() {
            combined_sim = (combined_sim + 0.2).min(1.0);
        }

        combined_sim
    }

    /// Detect category from market text. Order matters - politics is
    /// checked before sports to avoid "win the election" matching sports.
    pub fn categorize_market(&self, text: &str) -> &'static str {
        let lower = text.to_lowercase();

        if ["trump", "biden", "harris", "president", "election", "democrat", "republican", "congress", "senate", "governor", "mayor", "vote", "nominee", "primary", "presidential", "prime minister", "parliament"]
            .iter()
            .any(|x| lower.contains(x))
        {
            return "politics";
        }

        if ["bitcoin", "btc", "ethereum", "eth", "crypto", "token", "solana", "sol", "blockchain", "defi", "nft", "fdv", "market cap"].iter().any(|x| lower.contains(x)) {
            return "crypto";
        }

        if ["fed", "interest rate", "inflation", "gdp", "recession", "stock", "nasdaq", "dow", "s&p", "treasury", "tariff", "federal reserve"].iter().any(|x| lower.contains(x)) {
            return "finance";
        }

        let sports_keywords = ["nfl", "nba", "mlb", "nhl", "premier league", "champions league", "super bowl", "playoff", "la liga", "soccer", " fc", "basketball team", "football team", "hockey", "world cup", "stanley cup"];
        if sports_keywords.iter().any(|x| lower.contains(x)) {
            return "sports";
        }

        if TEAM_LOOKUP.keys().any(|k| lower.contains(k.as_str())) {
            return "sports";
        }

        if ["oscar", "grammy", "emmy", "movie", "film", "album", "artist", "actor", "actress", "netflix", "spotify", "best picture"].iter().any(|x| lower.contains(x)) {
            return "entertainment";
        }

        if ["ai ", "openai", "gpt", "google", "apple", "microsoft", "tesla", "spacex", "nvidia"].iter().any(|x| lower.contains(x)) {
            return "tech";
        }

        "other"
    }

    /// Find matching markets between platforms using category-based
    /// matching, calling `on_progress(checked, total, matches_found)`
    /// periodically so a caller can drive a progress UI.
    pub async fn find_matches(&mut self, polymarket_markets: &[Market], kalshi_markets: &[KalshiMarket], mut on_progress: Option<&mut (dyn FnMut(usize, usize, usize) + Send)>) -> Vec<MarketPair> {
        let mut matches = Vec::new();

        let active_poly: Vec<&Market> = polymarket_markets.iter().filter(|m| m.active).collect();
        let active_kalshi: Vec<&KalshiMarket> = kalshi_markets.iter().filter(|m| m.is_active()).collect();

        info!("Categorizing markets for faster matching...");

        let mut poly_by_cat: HashMap<&'static str, Vec<&Market>> = HashMap::new();
        for m in &active_poly {
            poly_by_cat.entry(self.categorize_market(&m.question)).or_default().push(m);
        }

        let mut kalshi_by_cat: HashMap<&'static str, Vec<&KalshiMarket>> = HashMap::new();
        for m in &active_kalshi {
            kalshi_by_cat.entry(self.categorize_market(&m.title)).or_default().push(m);
        }

        info!("=== CATEGORY BREAKDOWN ===");
        let all_cats: HashSet<&'static str> = poly_by_cat.keys().chain(kalshi_by_cat.keys()).copied().collect();
        for cat in &all_cats {
            info!(category = cat, polymarket = poly_by_cat.get(cat).map(Vec::len).unwrap_or(0), kalshi = kalshi_by_cat.get(cat).map(Vec::len).unwrap_or(0));
        }

        let total_comparisons: usize = all_cats.iter().map(|cat| poly_by_cat.get(cat).map(Vec::len).unwrap_or(0) * kalshi_by_cat.get(cat).map(Vec::len).unwrap_or(0)).sum();

        info!(total_comparisons, all_to_all = active_poly.len() * active_kalshi.len(), "Total comparisons (category-based)");

        let mut checked = 0usize;
        let priority_categories = ["sports", "politics", "crypto", "finance", "entertainment", "tech"];

        for category in priority_categories {
            let Some(poly_markets) = poly_by_cat.get(category) else { continue };
            let Some(kalshi_markets_cat) = kalshi_by_cat.get(category) else { continue };
            if poly_markets.is_empty() || kalshi_markets_cat.is_empty() {
                continue;
            }

            info!(category, poly = poly_markets.len(), kalshi = kalshi_markets_cat.len(), "Matching category");

            for poly_market in poly_markets {
                let mut best_match: Option<&KalshiMarket> = None;
                let mut best_score = 0.0;

                for kalshi_market in kalshi_markets_cat.iter() {
                    let score = self.calculate_similarity(&poly_market.question, &kalshi_market.title);
                    if score > best_score {
                        best_score = score;
                        best_match = Some(kalshi_market);
                    }
                    checked += 1;
                }

                if checked % 500 == 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    if checked % 5000 == 0 {
                        let pct = if total_comparisons > 0 { checked as f64 / total_comparisons as f64 * 100.0 } else { 0.0 };
                        info!(checked, total_comparisons, pct, matches = matches.len(), "Progress");
                    }
                    if let Some(cb) = on_progress.as_mut() {
                        cb(checked, total_comparisons, matches.len());
                    }
                }

                if let Some(best) = best_match {
                    if best_score >= self.min_similarity {
                        let pair = MarketPair {
                            polymarket_id: poly_market.market_id.clone(),
                            kalshi_ticker: best.ticker.clone(),
                            polymarket_question: poly_market.question.clone(),
                            kalshi_title: best.title.clone(),
                            similarity_score: best_score,
                            category: category.to_string(),
                            matched_at: Utc::now(),
                        };
                        info!(category, poly = %poly_market.question, kalshi = %best.title, score = best_score, "MATCHED");
                        self.matched_pairs.insert(pair.pair_id(), pair.clone());
                        matches.push(pair);
                    }
                }
            }
        }

        info!(pairs = matches.len(), "=== MATCHING COMPLETE ===");
        matches
    }

    pub fn get_cached_pairs(&self) -> Vec<MarketPair> {
        self.matched_pairs.values().cloned().collect()
    }
}
