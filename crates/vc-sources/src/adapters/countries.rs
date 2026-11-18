//! Country names → ISO 3166-1 alpha-2, for adapters whose sources name countries in English
//! (GOV.UK world locations, Wikipedia tables and article titles, medical-list titles).
//!
//! Exact lookups only, after folding (lowercase, accents removed, `&` → `and`, punctuation →
//! space, a leading "the" dropped, "St"/"St." → "saint"). A name that is not in the table
//! has no code: callers keep the name and leave the code out, they never guess.

use std::collections::HashMap;
use std::sync::LazyLock;

/// `(alpha-2, English short name)`, ISO 3166-1 plus Kosovo (`XK`, user-assigned, as used by
/// the EU and GOV.UK).
pub const COUNTRIES: &[(&str, &str)] = &[
    ("AD", "Andorra"),
    ("AE", "United Arab Emirates"),
    ("AF", "Afghanistan"),
    ("AG", "Antigua and Barbuda"),
    ("AI", "Anguilla"),
    ("AL", "Albania"),
    ("AM", "Armenia"),
    ("AO", "Angola"),
    ("AQ", "Antarctica"),
    ("AR", "Argentina"),
    ("AS", "American Samoa"),
    ("AT", "Austria"),
    ("AU", "Australia"),
    ("AW", "Aruba"),
    ("AZ", "Azerbaijan"),
    ("BA", "Bosnia and Herzegovina"),
    ("BB", "Barbados"),
    ("BD", "Bangladesh"),
    ("BE", "Belgium"),
    ("BF", "Burkina Faso"),
    ("BG", "Bulgaria"),
    ("BH", "Bahrain"),
    ("BI", "Burundi"),
    ("BJ", "Benin"),
    ("BL", "Saint Barthélemy"),
    ("BM", "Bermuda"),
    ("BN", "Brunei"),
    ("BO", "Bolivia"),
    ("BQ", "Caribbean Netherlands"),
    ("BR", "Brazil"),
    ("BS", "Bahamas"),
    ("BT", "Bhutan"),
    ("BW", "Botswana"),
    ("BY", "Belarus"),
    ("BZ", "Belize"),
    ("CA", "Canada"),
    ("CC", "Cocos (Keeling) Islands"),
    ("CD", "Democratic Republic of the Congo"),
    ("CF", "Central African Republic"),
    ("CG", "Republic of the Congo"),
    ("CH", "Switzerland"),
    ("CI", "Côte d'Ivoire"),
    ("CK", "Cook Islands"),
    ("CL", "Chile"),
    ("CM", "Cameroon"),
    ("CN", "China"),
    ("CO", "Colombia"),
    ("CR", "Costa Rica"),
    ("CU", "Cuba"),
    ("CV", "Cape Verde"),
    ("CW", "Curaçao"),
    ("CX", "Christmas Island"),
    ("CY", "Cyprus"),
    ("CZ", "Czech Republic"),
    ("DE", "Germany"),
    ("DJ", "Djibouti"),
    ("DK", "Denmark"),
    ("DM", "Dominica"),
    ("DO", "Dominican Republic"),
    ("DZ", "Algeria"),
    ("EC", "Ecuador"),
    ("EE", "Estonia"),
    ("EG", "Egypt"),
    ("EH", "Western Sahara"),
    ("ER", "Eritrea"),
    ("ES", "Spain"),
    ("ET", "Ethiopia"),
    ("FI", "Finland"),
    ("FJ", "Fiji"),
    ("FK", "Falkland Islands"),
    ("FM", "Micronesia"),
    ("FO", "Faroe Islands"),
    ("FR", "France"),
    ("GA", "Gabon"),
    ("GB", "United Kingdom"),
    ("GD", "Grenada"),
    ("GE", "Georgia"),
    ("GF", "French Guiana"),
    ("GG", "Guernsey"),
    ("GH", "Ghana"),
    ("GI", "Gibraltar"),
    ("GL", "Greenland"),
    ("GM", "Gambia"),
    ("GN", "Guinea"),
    ("GP", "Guadeloupe"),
    ("GQ", "Equatorial Guinea"),
    ("GR", "Greece"),
    ("GS", "South Georgia and the South Sandwich Islands"),
    ("GT", "Guatemala"),
    ("GU", "Guam"),
    ("GW", "Guinea-Bissau"),
    ("GY", "Guyana"),
    ("HK", "Hong Kong"),
    ("HN", "Honduras"),
    ("HR", "Croatia"),
    ("HT", "Haiti"),
    ("HU", "Hungary"),
    ("ID", "Indonesia"),
    ("IE", "Ireland"),
    ("IL", "Israel"),
    ("IM", "Isle of Man"),
    ("IN", "India"),
    ("IO", "British Indian Ocean Territory"),
    ("IQ", "Iraq"),
    ("IR", "Iran"),
    ("IS", "Iceland"),
    ("IT", "Italy"),
    ("JE", "Jersey"),
    ("JM", "Jamaica"),
    ("JO", "Jordan"),
    ("JP", "Japan"),
    ("KE", "Kenya"),
    ("KG", "Kyrgyzstan"),
    ("KH", "Cambodia"),
    ("KI", "Kiribati"),
    ("KM", "Comoros"),
    ("KN", "Saint Kitts and Nevis"),
    ("KP", "North Korea"),
    ("KR", "South Korea"),
    ("KW", "Kuwait"),
    ("KY", "Cayman Islands"),
    ("KZ", "Kazakhstan"),
    ("LA", "Laos"),
    ("LB", "Lebanon"),
    ("LC", "Saint Lucia"),
    ("LI", "Liechtenstein"),
    ("LK", "Sri Lanka"),
    ("LR", "Liberia"),
    ("LS", "Lesotho"),
    ("LT", "Lithuania"),
    ("LU", "Luxembourg"),
    ("LV", "Latvia"),
    ("LY", "Libya"),
    ("MA", "Morocco"),
    ("MC", "Monaco"),
    ("MD", "Moldova"),
    ("ME", "Montenegro"),
    ("MF", "Saint Martin"),
    ("MG", "Madagascar"),
    ("MH", "Marshall Islands"),
    ("MK", "North Macedonia"),
    ("ML", "Mali"),
    ("MM", "Myanmar"),
    ("MN", "Mongolia"),
    ("MO", "Macau"),
    ("MP", "Northern Mariana Islands"),
    ("MQ", "Martinique"),
    ("MR", "Mauritania"),
    ("MS", "Montserrat"),
    ("MT", "Malta"),
    ("MU", "Mauritius"),
    ("MV", "Maldives"),
    ("MW", "Malawi"),
    ("MX", "Mexico"),
    ("MY", "Malaysia"),
    ("MZ", "Mozambique"),
    ("NA", "Namibia"),
    ("NC", "New Caledonia"),
    ("NE", "Niger"),
    ("NF", "Norfolk Island"),
    ("NG", "Nigeria"),
    ("NI", "Nicaragua"),
    ("NL", "Netherlands"),
    ("NO", "Norway"),
    ("NP", "Nepal"),
    ("NR", "Nauru"),
    ("NU", "Niue"),
    ("NZ", "New Zealand"),
    ("OM", "Oman"),
    ("PA", "Panama"),
    ("PE", "Peru"),
    ("PF", "French Polynesia"),
    ("PG", "Papua New Guinea"),
    ("PH", "Philippines"),
    ("PK", "Pakistan"),
    ("PL", "Poland"),
    ("PM", "Saint Pierre and Miquelon"),
    ("PN", "Pitcairn Islands"),
    ("PR", "Puerto Rico"),
    ("PS", "Palestine"),
    ("PT", "Portugal"),
    ("PW", "Palau"),
    ("PY", "Paraguay"),
    ("QA", "Qatar"),
    ("RE", "Réunion"),
    ("RO", "Romania"),
    ("RS", "Serbia"),
    ("RU", "Russia"),
    ("RW", "Rwanda"),
    ("SA", "Saudi Arabia"),
    ("SB", "Solomon Islands"),
    ("SC", "Seychelles"),
    ("SD", "Sudan"),
    ("SE", "Sweden"),
    ("SG", "Singapore"),
    ("SH", "Saint Helena, Ascension and Tristan da Cunha"),
    ("SI", "Slovenia"),
    ("SK", "Slovakia"),
    ("SL", "Sierra Leone"),
    ("SM", "San Marino"),
    ("SN", "Senegal"),
    ("SO", "Somalia"),
    ("SR", "Suriname"),
    ("SS", "South Sudan"),
    ("ST", "São Tomé and Príncipe"),
    ("SV", "El Salvador"),
    ("SX", "Sint Maarten"),
    ("SY", "Syria"),
    ("SZ", "Eswatini"),
    ("TC", "Turks and Caicos Islands"),
    ("TD", "Chad"),
    ("TG", "Togo"),
    ("TH", "Thailand"),
    ("TJ", "Tajikistan"),
    ("TK", "Tokelau"),
    ("TL", "Timor-Leste"),
    ("TM", "Turkmenistan"),
    ("TN", "Tunisia"),
    ("TO", "Tonga"),
    ("TR", "Turkey"),
    ("TT", "Trinidad and Tobago"),
    ("TV", "Tuvalu"),
    ("TW", "Taiwan"),
    ("TZ", "Tanzania"),
    ("UA", "Ukraine"),
    ("UG", "Uganda"),
    ("US", "United States"),
    ("UY", "Uruguay"),
    ("UZ", "Uzbekistan"),
    ("VA", "Vatican City"),
    ("VC", "Saint Vincent and the Grenadines"),
    ("VE", "Venezuela"),
    ("VG", "British Virgin Islands"),
    ("VI", "United States Virgin Islands"),
    ("VN", "Vietnam"),
    ("VU", "Vanuatu"),
    ("WF", "Wallis and Futuna"),
    ("WS", "Samoa"),
    ("XK", "Kosovo"),
    ("YE", "Yemen"),
    ("YT", "Mayotte"),
    ("ZA", "South Africa"),
    ("ZM", "Zambia"),
    ("ZW", "Zimbabwe"),
];

/// Other English names for the same codes (GOV.UK world locations, Wikipedia, Global Affairs
/// Canada). Folded on load, like the names above.
const ALIASES: &[(&str, &str)] = &[
    ("AE", "UAE"),
    ("BN", "Brunei Darussalam"),
    ("BO", "Plurinational State of Bolivia"),
    ("BS", "The Bahamas"),
    ("CD", "Congo, Democratic Republic"),
    ("CD", "Democratic Republic of Congo"),
    ("CD", "DR Congo"),
    ("CD", "Congo (Kinshasa)"),
    ("CG", "Congo"),
    ("CG", "Congo (Brazzaville)"),
    ("CG", "Republic of Congo"),
    ("CI", "Ivory Coast"),
    ("CN", "People's Republic of China"),
    ("CV", "Cabo Verde"),
    ("CZ", "Czechia"),
    ("FK", "Falkland Islands (Islas Malvinas)"),
    ("FM", "Federated States of Micronesia"),
    ("GB", "UK"),
    ("GB", "Great Britain"),
    ("GB", "United Kingdom of Great Britain and Northern Ireland"),
    ("GM", "The Gambia"),
    ("HK", "Hong Kong SAR"),
    ("IR", "Islamic Republic of Iran"),
    ("KP", "Democratic People's Republic of Korea"),
    ("KR", "Republic of Korea"),
    ("KR", "Korea, South"),
    ("KP", "Korea, North"),
    ("LA", "Lao PDR"),
    ("LA", "Lao People's Democratic Republic"),
    ("MD", "Republic of Moldova"),
    ("MK", "Macedonia"),
    ("MK", "Republic of North Macedonia"),
    ("MM", "Burma"),
    ("MM", "Myanmar (Burma)"),
    ("MO", "Macao"),
    ("MO", "Macau SAR"),
    ("NL", "The Netherlands"),
    ("PH", "The Philippines"),
    ("PN", "Pitcairn"),
    ("PN", "Pitcairn Island"),
    ("PS", "Palestinian Territories"),
    ("PS", "The Occupied Palestinian Territories"),
    ("PS", "Occupied Palestinian Territories"),
    ("PS", "State of Palestine"),
    ("PS", "West Bank and Gaza"),
    ("RU", "Russian Federation"),
    ("SH", "Saint Helena"),
    ("SH", "St Helena, Ascension and Tristan da Cunha"),
    ("SY", "Syrian Arab Republic"),
    ("SZ", "Swaziland"),
    ("SZ", "Eswatini (Swaziland)"),
    ("TL", "East Timor"),
    ("TW", "Republic of China"),
    ("TW", "Taiwan (Republic of China)"),
    ("TR", "Türkiye"),
    ("TR", "Turkiye"),
    ("TZ", "United Republic of Tanzania"),
    ("US", "USA"),
    ("US", "United States of America"),
    ("US", "US"),
    ("VA", "Holy See"),
    ("VA", "The Holy See"),
    ("VA", "Vatican"),
    ("VE", "Bolivarian Republic of Venezuela"),
    ("VI", "US Virgin Islands"),
    ("VI", "U.S. Virgin Islands"),
    ("VN", "Viet Nam"),
];

/// Folds a country name for lookup.
pub fn fold(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.trim().chars() {
        match c {
            'á' | 'à' | 'â' | 'ä' | 'ã' | 'å' | 'Á' | 'À' | 'Â' | 'Ä' | 'Ã' | 'Å' => {
                s.push('a')
            }
            'é' | 'è' | 'ê' | 'ë' | 'É' | 'È' | 'Ê' | 'Ë' => s.push('e'),
            'í' | 'ì' | 'î' | 'ï' | 'Í' | 'Ì' | 'Î' | 'Ï' => s.push('i'),
            'ó' | 'ò' | 'ô' | 'ö' | 'õ' | 'Ó' | 'Ò' | 'Ô' | 'Ö' | 'Õ' => s.push('o'),
            'ú' | 'ù' | 'û' | 'ü' | 'Ú' | 'Ù' | 'Û' | 'Ü' => s.push('u'),
            'ç' | 'Ç' => s.push('c'),
            'ñ' | 'Ñ' => s.push('n'),
            '&' => s.push_str(" and "),
            '\'' | '’' | '‘' => {}
            c if c.is_alphanumeric() => s.extend(c.to_lowercase()),
            _ => s.push(' '),
        }
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    let words = match words.first() {
        Some(&"the") => &words[1..],
        _ => &words[..],
    };
    words
        .iter()
        .map(|w| if *w == "st" { "saint" } else { w })
        .collect::<Vec<_>>()
        .join(" ")
}

static BY_NAME: LazyLock<HashMap<String, &'static str>> = LazyLock::new(|| {
    let mut m = HashMap::new();
    for (code, name) in COUNTRIES.iter().chain(ALIASES) {
        m.insert(fold(name), *code);
    }
    m
});

/// The alpha-2 code of an English country name, if the table knows it.
pub fn iso2(name: &str) -> Option<&'static str> {
    BY_NAME.get(&fold(name)).copied()
}

/// The English short name of an alpha-2 code.
pub fn name(code: &str) -> Option<&'static str> {
    COUNTRIES
        .iter()
        .find(|(c, _)| c.eq_ignore_ascii_case(code))
        .map(|(_, n)| *n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_aliases_resolve() {
        assert_eq!(iso2("Thailand"), Some("TH"));
        assert_eq!(iso2("The Occupied Palestinian Territories"), Some("PS"));
        assert_eq!(iso2("Côte d’Ivoire"), Some("CI"));
        assert_eq!(iso2("Cote d'Ivoire"), Some("CI"));
        assert_eq!(iso2("St Kitts and Nevis"), Some("KN"));
        assert_eq!(iso2("St. Lucia"), Some("LC"));
        assert_eq!(iso2("Bosnia & Herzegovina"), Some("BA"));
        assert_eq!(iso2("Türkiye"), Some("TR"));
        assert_eq!(iso2("the bahamas"), Some("BS"));
        assert_eq!(iso2("Atlantis"), None);
        assert_eq!(name("gb"), Some("United Kingdom"));
    }

    #[test]
    fn codes_are_unique_and_upper_case() {
        let mut seen = std::collections::HashSet::new();
        for (c, _) in COUNTRIES {
            assert!(
                c.len() == 2 && c.bytes().all(|b| b.is_ascii_uppercase()),
                "{c}"
            );
            assert!(seen.insert(*c), "duplicate {c}");
        }
        for (c, a) in ALIASES {
            assert!(seen.contains(c), "alias {a} → unknown {c}");
        }
    }
}
