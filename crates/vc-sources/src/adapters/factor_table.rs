//! `factor_table`: published emission-factor and grid-intensity tables (CSV, zipped CSV, XLSX
//! or ODS) for the trip carbon footprint (2026-11-19), driven by a [`FactorMapping`] per
//! dataset table.
//!
//! One leased row = one file: a single request (conditional on the stored validators, capped
//! at the mapping's `max_bytes`), spooled raw as a WARC `response` (raw first). Nothing is
//! derived at fetch time beyond a parse check; the processor calls [`items`] on the stored
//! body and every mapped row becomes one `emission_factor` record, one
//! `grid_carbon_intensity` observation or one `occupancy` record (persons per vehicle). No
//! LLM, no unit conversion: values and units are copied as published.
//!
//! A file may hold several tables (one sheet per table, or a stack of tables on one sheet):
//! every mapping whose URL test matches the file is applied ([`mappings_for`]), each with its
//! own dataset id. A stacked table is found by its `anchor` (the "Table 10" title cell above
//! it) and ends at the first row whose label cell is empty or a "Source"/"Notes" line.
//!
//! **Adding a dataset:** what differs per dataset is data, not code. Add a mapping (how to
//! recognise the file's URL, format and sheet, the table anchor, the header's required columns,
//! row filters, the columns that make the activity label, the value and year columns, the unit,
//! and the category / gas-basis tables), record a small fixture excerpt, and point the source's
//! `adapter` at `factor_table` with the file URL as its seed.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;

use async_trait::async_trait;
use serde_json::{Map, Value, json};
use vc_core::media::MediaType;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};
use vc_core::{CanonicalUrl, Error, Result};

use super::common::{Fetch, check_status, fetch_raw, not_modified};
use super::zip_stream::ZipStream;
use super::{countries, iso3};

/// Adapter id.
pub const ID: &str = "factor_table";

/// Bumped when a mapping's output changes.
pub const FILTER_VERSION: &str = "factor-table-v3";

/// Largest decompressed member of a zipped CSV (zip-bomb guard).
pub const MAX_UNZIPPED_BYTES: usize = 64 * 1024 * 1024;

/// File format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Comma-separated values; the header is the first row that holds every required column.
    Csv,
    /// A ZIP archive holding one CSV member (base name, ASCII case-insensitive).
    ZipCsv {
        /// Member names accepted.
        member: &'static [&'static str],
    },
    /// An Excel workbook (`.xlsx`, `.xlsm`): the named sheet.
    Xlsx {
        /// Sheet name.
        sheet: &'static str,
    },
    /// An OpenDocument spreadsheet (`.ods`): the named sheet.
    Ods {
        /// Sheet name.
        sheet: &'static str,
    },
}

/// How a column is found in the header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    /// Not provided.
    None,
    /// Exactly this header.
    Name(&'static str),
    /// The first header starting with this text (e.g. `"GHG Conversion Factor "` + year).
    Prefix(&'static str),
}

/// A row filter: keep the row only when the column's value is one of `values` (exact,
/// case-sensitive, after trimming).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Keep {
    /// Column.
    pub column: &'static str,
    /// Accepted values.
    pub values: &'static [&'static str],
}

/// Where the year comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Year {
    /// A column holding the year.
    Column(&'static str),
    /// The first year (19xx/20xx) written in a column (`"Apr 2024 to Mar 2025 [p]"` → 2024).
    FirstYearIn(&'static str),
    /// The four digits at the end of the value column's header (DESNZ
    /// `"GHG Conversion Factor 2026"`).
    ValueHeaderSuffix,
    /// The four digits right after this text in the rows above the header (eGRID's title
    /// `"1. Subregion Output Emission Rates (eGRID2023)"`).
    Preamble(&'static str),
    /// The last year (19xx/20xx, not followed by another digit) in the file name
    /// (`ghg-emission-factors-hub-2025.xlsx`).
    FileName,
}

/// Where the geography comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Geo {
    /// Always this geography (`"global"`, `"United Kingdom"`), with its code if one country.
    Fixed(&'static str, &'static str),
    /// From an English country-name column for rows whose category is one of `when`, else the
    /// fixed fallback.
    CountryColumn {
        /// Column holding the country name.
        column: &'static str,
        /// Categories the column applies to.
        when: &'static [&'static str],
        /// Fallback geography name and code.
        fallback: (&'static str, &'static str),
    },
    /// An ISO 3166-1 alpha-3 column (`code`), with the name column; rows without a valid
    /// country code (aggregates, regions) are skipped.
    Iso3 {
        /// Alpha-3 column.
        code: &'static str,
        /// Name column.
        name: &'static str,
    },
    /// A sub-national region of one country, by the publisher's code (eGRID subregion `CAMX`,
    /// US state `CA`), stored as `region_code`; the row whose code is `national` is the
    /// country total.
    Region {
        /// Column holding the region code.
        code: &'static str,
        /// Column holding the region name (or `""`).
        name: &'static str,
        /// What the code names (`"eGRID subregion"`, `"US state"`).
        label: &'static str,
        /// The country (name, ISO alpha-2).
        country: (&'static str, &'static str),
        /// The code of the country-total row.
        national: &'static str,
        /// Whether the country-total row becomes a record (`false` when another table of the
        /// same file already carries the same total, so it is stored once).
        emit_national: bool,
        /// Codes that name a territory with its own ISO 3166-1 code, as `(code, alpha-2,
        /// name, total)`: stored under that country, as its total when `total` is set, else
        /// with the code as `region_code` (eGRID's Puerto Rico rows).
        own_country: &'static [(&'static str, &'static str, &'static str, bool)],
    },
}

/// What a mapped row becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// `emission_factor` records.
    Factor {
        /// A fixed text that leads the activity label (or `""`).
        activity_prefix: &'static str,
        /// Columns joined with `" / "` into the activity label (empty cells skipped).
        activity: &'static [&'static str],
        /// Drop a trailing footnote letter from the activity cells (`"Passenger Car A"`).
        footnotes: bool,
        /// `(substring of "<category columns>", category)`: the first match wins; no match →
        /// the row is skipped.
        categories: &'static [(&'static str, &'static str)],
        /// Columns joined with `" / "` that the category table is matched against.
        category_from: &'static [&'static str],
        /// `(substring of the activity label, gas basis)`: first match wins, else `default`.
        gas_basis: &'static [(&'static str, &'static str)],
        /// Gas basis when no rule matches.
        gas_default: &'static str,
        /// Unit numerator (`"kg CO2e"`) as a fixed text, or from this column when non-empty.
        unit_mass: (&'static str, &'static str),
        /// Unit denominator as a fixed text, or from this column when non-empty.
        unit_per: (&'static str, &'static str),
        /// Scope as a fixed text, or from this column when non-empty.
        scope: (&'static str, &'static str),
        /// The publisher's row id column (or `""`).
        factor_id: &'static str,
        /// Geography.
        geo: Geo,
    },
    /// `grid_carbon_intensity` observations (annual, life-cycle unless stated).
    GridIntensity {
        /// Geography (an [`Geo::Iso3`] column pair).
        geo: Geo,
        /// `lifecycle` or `direct`.
        basis: &'static str,
    },
    /// `occupancy` records (average persons per vehicle). Only the rows listed in `rows` are
    /// mapped; every other row of the table (goods vehicles, partial splits) is skipped.
    Occupancy {
        /// Columns joined with `" / "` into the row's label (`"Car / Average Car"`).
        label: &'static [&'static str],
        /// `(row label, mode, trip purpose)`: exact match on the label.
        rows: &'static [(&'static str, &'static str, &'static str)],
        /// `per_vehicle_km` or `per_trip`.
        basis: &'static str,
        /// Whether the published figures count the driver.
        includes_driver: bool,
        /// `all_week`, `weekday`, `weekend` (or `""`).
        time_period: &'static str,
        /// Geography ([`Geo::Fixed`]).
        geo: Geo,
        /// `as_of` from the first cell above the header that starts with "<Month> <year>"
        /// (TAG's `"May 2026 v2.03"` → `2026-05-01`).
        as_of_from_preamble: bool,
    },
}

/// One dataset table.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FactorMapping {
    /// Short id of the dataset table (record-id prefix; unique).
    pub dataset: &'static str,
    /// The dataset's name and edition, as stored in `dataset`.
    pub dataset_label: &'static str,
    /// Publisher.
    pub publisher: &'static str,
    /// The file row's URL path must contain every one of these.
    pub path_contains: &'static [&'static str],
    /// Format.
    pub format: Format,
    /// The title cell of a stacked table (`"Table 10"`, exact after trimming), or `""` for a
    /// sheet that holds one table. With an anchor, the table ends at the first row whose first
    /// required cell is empty or starts with "Source" or "Note".
    pub anchor: &'static str,
    /// Header rows: `1`, or `2` when the column names are split over two rows (a merged
    /// "Weekend" / "All Week" above a row of time bands): the header row is then the first one
    /// that, joined cell by cell with the row above it, holds every required column.
    pub header_rows: u8,
    /// Columns whose empty cells take the value above them (a merged group label such as
    /// TAG's "Vehicle Type"); a cell in parentheses (`"(Occupants)"`) is a note under the
    /// group label and is filled over too.
    pub fill_down: &'static [&'static str],
    /// Columns the header must have (else the file is refused, nothing mapped).
    pub required: &'static [&'static str],
    /// Row filters (all must accept).
    pub keep: &'static [Keep],
    /// The value column.
    pub value: Col,
    /// The year.
    pub year: Year,
    /// Rows of earlier years are skipped (0 = keep all).
    pub min_year: i64,
    /// A published 0 means "no figure" in this dataset (skipped, never stored as 0).
    pub skip_zero: bool,
    /// Body cap in bytes.
    pub max_bytes: u64,
    /// Row → record.
    pub output: Output,
}

const DESNZ_CATEGORIES: &[(&str, &str)] = &[
    ("Business travel- air / Flights", "flights"),
    ("Business travel- sea / Ferry", "ferry"),
    ("Business travel- land / Rail", "rail"),
    ("Business travel- land / Bus", "bus"),
    ("Business travel- land / Taxis", "taxi"),
    ("Business travel- land / Motorbike", "motorbike"),
    ("Business travel- land / Cars", "car"),
    ("Hotel stay / Hotel stay", "accommodation"),
    ("UK electricity / Electricity generated", "electricity"),
];

const OWID_TRAVEL_CATEGORIES: &[(&str, &str)] = &[
    ("Coach", "coach"),
    ("flight", "flights"),
    ("Eurostar", "rail"),
    ("rail", "rail"),
    ("Underground", "rail"),
    ("Tram", "rail"),
    ("Bus", "bus"),
    ("Ferry", "ferry"),
    ("Motorbike", "motorbike"),
    ("car", "car"),
    ("hybrid", "car"),
];

const EF_HUB_TRAVEL_CATEGORIES: &[(&str, &str)] = &[
    ("Air Travel", "flights"),
    ("Rail", "rail"),
    ("Bus", "bus"),
    ("Motorcycle", "motorbike"),
    ("Passenger Car", "car"),
    ("Light-Duty Truck", "car"),
];

const OTHER: &[(&str, &str)] = &[("", "other")];
const FOOD: &[(&str, &str)] = &[("", "food")];

const OWID_CO2_PATH: &[&str] = &["/owid/co2-data/", "owid-co2-data.csv"];
const OWID_CO2: &str = "Our World in Data CO2 and greenhouse gas emissions dataset";
const OWID_CO2_PUBLISHER: &str =
    "Our World in Data, based on the Global Carbon Budget and Jones et al.";
const OWID_CO2_MAX: u64 = 24 * 1024 * 1024;
/// Earliest year of the OWID per-capita rows that are stored.
pub const OWID_CO2_FROM: i64 = 2010;

const EGRID_PATH: &[&str] = &["/system/files/", "summary_tables", ".xlsx"];
const EGRID: &str = "US EPA eGRID summary tables (output emission rates)";
const EGRID_PREFIX: &str = "Electricity generation, total output emission rate";

const EF_HUB_PATH: &[&str] = &["ghg-emission-factors-hub-", ".xlsx"];
const EF_HUB_SHEET: Format = Format::Xlsx {
    sheet: "Emission Factors Hub",
};
const EF_HUB: &str = "US EPA GHG Emission Factors Hub";
const EPA: &str = "US Environmental Protection Agency (EPA)";
const US: Geo = Geo::Fixed("United States", "US");

const ORR_VALUE: &str =
    "Carbon dioxide equivalent emissions from traction energy (grams per passenger kilometre)";

const CONCITO_PATH: &[&str] = &["/files/media/document/", "_ENG_rev_", ".xlsx"];
const CONCITO_REQUIRED: &[&str] = &["ID_Ra", "Name", "Category", "Total kg CO2-eq/kg"];
const CONCITO_PUBLISHER: &str = "CONCITO and 2-0 LCA (The Big Climate Database)";

const RIVM_PATH: &[&str] = &[
    "Database-milieubelasting-voedingsmiddelen-database-versie-",
    ".xlsx",
];
const RIVM_PUBLISHER: &str =
    "Dutch National Institute for Public Health and the Environment (RIVM)";

const fn concito(
    dataset: &'static str,
    label: &'static str,
    sheet: &'static str,
    geo: Geo,
) -> FactorMapping {
    FactorMapping {
        dataset,
        dataset_label: label,
        publisher: CONCITO_PUBLISHER,
        path_contains: CONCITO_PATH,
        format: Format::Xlsx { sheet },
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: CONCITO_REQUIRED,
        keep: &[],
        value: Col::Name("Total kg CO2-eq/kg"),
        year: Year::FileName,
        min_year: 0,
        skip_zero: true,
        max_bytes: 4 * 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "",
            activity: &["Name"],
            footnotes: false,
            categories: FOOD,
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2e",
            unit_mass: ("kg CO2e", ""),
            unit_per: ("kg", ""),
            scope: ("cradle to retail, incl. iLUC", ""),
            factor_id: "ID_Ra",
            geo,
        },
    }
}

const fn rivm(
    dataset: &'static str,
    label: &'static str,
    sheet: &'static str,
    scope: &'static str,
) -> FactorMapping {
    FactorMapping {
        dataset,
        dataset_label: label,
        publisher: RIVM_PUBLISHER,
        path_contains: RIVM_PATH,
        format: Format::Xlsx { sheet },
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &["Naam", "NEVO code"],
        keep: &[],
        value: Col::Prefix("Global warming"),
        year: Year::FileName,
        min_year: 0,
        skip_zero: false,
        max_bytes: 4 * 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "",
            activity: &["Naam"],
            footnotes: false,
            categories: FOOD,
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2e",
            unit_mass: ("kg CO2e", ""),
            unit_per: ("kg", ""),
            scope: (scope, ""),
            factor_id: "",
            geo: Geo::Fixed("Netherlands", "NL"),
        },
    }
}

const fn owid_co2(
    dataset: &'static str,
    required: &'static [&'static str],
    column: &'static str,
    prefix: &'static str,
    unit_mass: &'static str,
    gas: &'static str,
    scope: &'static str,
) -> FactorMapping {
    FactorMapping {
        dataset,
        dataset_label: OWID_CO2,
        publisher: OWID_CO2_PUBLISHER,
        path_contains: OWID_CO2_PATH,
        format: Format::Csv,
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required,
        keep: &[],
        value: Col::Name(column),
        year: Year::Column("year"),
        min_year: OWID_CO2_FROM,
        skip_zero: false,
        max_bytes: OWID_CO2_MAX,
        output: Output::Factor {
            activity_prefix: prefix,
            activity: &["country"],
            footnotes: false,
            categories: OTHER,
            category_from: &[],
            gas_basis: &[],
            gas_default: gas,
            unit_mass: (unit_mass, ""),
            unit_per: ("person per year", ""),
            scope: (scope, ""),
            factor_id: "",
            geo: Geo::Iso3 {
                code: "iso_code",
                name: "country",
            },
        },
    }
}

/// Every built-in mapping.
pub const MAPPINGS: &[FactorMapping] = &[
    // UK DESNZ GHG conversion factors, flat file (yearly in June, OGL v3): flights by haul and
    // cabin (with and without RF), ferry, rail, bus, taxi, motorbike, cars, hotel stays per
    // country, UK grid electricity. Only kg CO2e rows (not the per-gas splits).
    FactorMapping {
        dataset: "desnz",
        dataset_label: "UK GHG conversion factors for company reporting, flat file",
        publisher: "UK Department for Energy Security and Net Zero (DESNZ)",
        path_contains: &["ghg-conversion-factors-", "flat-format"],
        format: Format::Xlsx {
            sheet: "Factors by Category",
        },
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &[
            "ID",
            "Scope",
            "Level 1",
            "Level 2",
            "Level 3",
            "Level 4",
            "Column Text",
            "UOM",
            "GHG/Unit",
        ],
        keep: &[
            Keep {
                column: "Level 1",
                values: &[
                    "Business travel- air",
                    "Business travel- sea",
                    "Business travel- land",
                    "Hotel stay",
                    "UK electricity",
                ],
            },
            Keep {
                column: "GHG/Unit",
                values: &["kg CO2e"],
            },
        ],
        value: Col::Prefix("GHG Conversion Factor "),
        year: Year::ValueHeaderSuffix,
        min_year: 0,
        skip_zero: false,
        max_bytes: 8 * 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "",
            activity: &["Level 2", "Level 3", "Level 4", "Column Text"],
            footnotes: false,
            categories: DESNZ_CATEGORIES,
            category_from: &["Level 1", "Level 2"],
            gas_basis: &[
                ("With RF", "co2e_with_rf"),
                ("Without RF", "co2e_without_rf"),
            ],
            gas_default: "co2e",
            unit_mass: ("kg CO2e", "GHG/Unit"),
            unit_per: ("", "UOM"),
            scope: ("", "Scope"),
            factor_id: "ID",
            geo: Geo::CountryColumn {
                column: "Level 3",
                when: &["accommodation"],
                fallback: ("United Kingdom", "GB"),
            },
        },
    },
    // Our World in Data grapher: GHG emissions per kg of food product (Poore & Nemecek 2018,
    // global means, farm to retail), CC BY 4.0.
    FactorMapping {
        dataset: "owid-food-poore",
        dataset_label: "Our World in Data: greenhouse gas emissions per kilogram of food product (Poore & Nemecek 2018)",
        publisher: "Our World in Data, based on Poore & Nemecek (2018)",
        path_contains: &["/grapher/ghg-per-kg-poore.csv"],
        format: Format::Csv,
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &[
            "entity",
            "year",
            "ghg_emissions_per_kilogram__poore__and__nemecek__2018",
        ],
        keep: &[],
        value: Col::Name("ghg_emissions_per_kilogram__poore__and__nemecek__2018"),
        year: Year::Column("year"),
        min_year: 0,
        skip_zero: false,
        max_bytes: 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "",
            activity: &["entity"],
            footnotes: false,
            categories: FOOD,
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2e",
            unit_mass: ("kg CO2e", ""),
            unit_per: ("kg", ""),
            scope: ("", ""),
            factor_id: "",
            geo: Geo::Fixed("global", ""),
        },
    },
    // Our World in Data grapher: life-cycle carbon intensity of electricity per country and
    // year (Ember and Energy Institute), CC BY 4.0.
    FactorMapping {
        dataset: "owid-grid-ci",
        dataset_label: "Our World in Data: carbon intensity of electricity generation",
        publisher: "Our World in Data, based on Ember and Energy Institute",
        path_contains: &["/grapher/electricity-mix.csv"],
        format: Format::Csv,
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &["entity", "code", "year", "co2_intensity__gco2_kwh"],
        keep: &[],
        value: Col::Name("co2_intensity__gco2_kwh"),
        year: Year::Column("year"),
        min_year: 0,
        skip_zero: false,
        max_bytes: 4 * 1024 * 1024,
        output: Output::GridIntensity {
            geo: Geo::Iso3 {
                code: "code",
                name: "entity",
            },
            basis: "lifecycle",
        },
    },
    // Our World in Data grapher: carbon footprint of travel per km by mode (the UK DESNZ
    // factors as OWID publishes them; g CO2e per passenger-km, flights with the RF uplift),
    // CC BY 4.0.
    FactorMapping {
        dataset: "owid-travel-mode",
        dataset_label: "Our World in Data: carbon footprint of travel per kilometre (UK DESNZ factors)",
        publisher: "Our World in Data, based on UK DESNZ",
        path_contains: &["/grapher/carbon-footprint-travel-mode.csv"],
        format: Format::Csv,
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &["entity", "year", "transport_emissions_factor"],
        keep: &[],
        value: Col::Name("transport_emissions_factor"),
        year: Year::Column("year"),
        min_year: 0,
        skip_zero: false,
        max_bytes: 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "",
            activity: &["entity"],
            footnotes: false,
            categories: OWID_TRAVEL_CATEGORIES,
            category_from: &["entity"],
            gas_basis: &[("flight", "co2e_with_rf")],
            gas_default: "co2e",
            unit_mass: ("g CO2e", ""),
            unit_per: ("passenger.km", ""),
            // A secondary copy: od-desnz-ghg-factors stores the same factors at source.
            scope: ("secondary copy of UK DESNZ factors", ""),
            factor_id: "",
            geo: Geo::Fixed("United Kingdom", "GB"),
        },
    },
    // Our World in Data CO2 dataset (CC BY 4.0): per-capita CO2 (territorial and
    // consumption-based) and GHG per country, from 2010, for destination comparisons.
    owid_co2(
        "owid-co2-per-capita",
        &["country", "year", "iso_code", "co2_per_capita"],
        "co2_per_capita",
        "CO2 emissions per capita",
        "t CO2",
        "co2",
        "territorial, fossil fuels and industry",
    ),
    owid_co2(
        "owid-co2-consumption-per-capita",
        &["country", "year", "iso_code", "consumption_co2_per_capita"],
        "consumption_co2_per_capita",
        "Consumption-based CO2 emissions per capita",
        "t CO2",
        "co2",
        "consumption-based",
    ),
    owid_co2(
        "owid-ghg-per-capita",
        &["country", "year", "iso_code", "ghg_per_capita"],
        "ghg_per_capita",
        "Greenhouse gas emissions per capita",
        "t CO2e",
        "co2e",
        "territorial, incl. land use",
    ),
    // US EPA eGRID summary tables (US public domain): Table 1, subregion output emission
    // rates, and Table 3, state output emission rates (lb CO2e per MWh, total output). The
    // series key of grid_carbon_intensity is per country, so sub-national rates are
    // emission_factor records with a region_code.
    FactorMapping {
        dataset: "egrid-subregion",
        dataset_label: EGRID,
        publisher: EPA,
        path_contains: EGRID_PATH,
        format: Format::Xlsx { sheet: "Table 1" },
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &[
            "eGRID subregion acronym",
            "eGRID subregion name",
            "CO₂",
            "CO₂e",
        ],
        keep: &[],
        value: Col::Name("CO₂e"),
        year: Year::Preamble("eGRID"),
        min_year: 0,
        skip_zero: false,
        max_bytes: 4 * 1024 * 1024,
        output: Output::Factor {
            activity_prefix: EGRID_PREFIX,
            activity: &["eGRID subregion name"],
            footnotes: false,
            categories: &[("", "electricity")],
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2e",
            unit_mass: ("lb CO2e", ""),
            unit_per: ("MWh", ""),
            scope: ("total output emission rate", ""),
            factor_id: "eGRID subregion acronym",
            geo: Geo::Region {
                code: "eGRID subregion acronym",
                name: "eGRID subregion name",
                label: "eGRID subregion",
                country: ("United States", "US"),
                national: "U.S.",
                // Table 3 carries the same U.S. total; it is stored from there only.
                emit_national: false,
                own_country: &[("PRMS", "PR", "Puerto Rico", false)],
            },
        },
    },
    FactorMapping {
        dataset: "egrid-state",
        dataset_label: EGRID,
        publisher: EPA,
        path_contains: EGRID_PATH,
        format: Format::Xlsx { sheet: "Table 3" },
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &["State", "CO₂", "CO₂e"],
        keep: &[],
        value: Col::Name("CO₂e"),
        year: Year::Preamble("eGRID"),
        min_year: 0,
        skip_zero: false,
        max_bytes: 4 * 1024 * 1024,
        output: Output::Factor {
            activity_prefix: EGRID_PREFIX,
            activity: &[],
            footnotes: false,
            categories: &[("", "electricity")],
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2e",
            unit_mass: ("lb CO2e", ""),
            unit_per: ("MWh", ""),
            scope: ("total output emission rate", ""),
            factor_id: "State",
            geo: Geo::Region {
                code: "State",
                name: "",
                label: "US state",
                country: ("United States", "US"),
                national: "U.S.",
                emit_national: true,
                own_country: &[("PR", "PR", "Puerto Rico", true)],
            },
        },
    },
    // US EPA GHG Emission Factors Hub (yearly in January, US public domain), one sheet of
    // stacked tables: Table 10 (business travel and commuting, kg CO2 per vehicle- or
    // passenger-mile; CH4 and N2O are separate columns, not CO2e) and Table 2 (mobile
    // combustion, kg CO2 per unit of fuel).
    FactorMapping {
        dataset: "epa-ef-hub-travel",
        dataset_label: EF_HUB,
        publisher: EPA,
        path_contains: EF_HUB_PATH,
        format: EF_HUB_SHEET,
        anchor: "Table 10",
        header_rows: 1,
        fill_down: &[],
        required: &["Vehicle Type", "Units"],
        keep: &[],
        value: Col::Prefix("CO2 Factor"),
        year: Year::FileName,
        min_year: 0,
        skip_zero: false,
        max_bytes: 4 * 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "",
            activity: &["Vehicle Type"],
            footnotes: true,
            categories: EF_HUB_TRAVEL_CATEGORIES,
            category_from: &["Vehicle Type"],
            gas_basis: &[],
            gas_default: "co2",
            unit_mass: ("kg CO2", ""),
            unit_per: ("", "Units"),
            scope: ("Scope 3 cat. 6 and 7, tank-to-wheel", ""),
            factor_id: "",
            geo: US,
        },
    },
    FactorMapping {
        dataset: "epa-ef-hub-mobile-co2",
        dataset_label: EF_HUB,
        publisher: EPA,
        path_contains: EF_HUB_PATH,
        format: EF_HUB_SHEET,
        anchor: "Table 2",
        header_rows: 1,
        fill_down: &[],
        required: &["Fuel Type", "kg CO2 per unit", "Unit"],
        keep: &[],
        value: Col::Name("kg CO2 per unit"),
        year: Year::FileName,
        min_year: 0,
        skip_zero: false,
        max_bytes: 4 * 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "Mobile combustion",
            activity: &["Fuel Type"],
            footnotes: false,
            categories: &[("", "fuel")],
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2",
            unit_mass: ("kg CO2", ""),
            unit_per: ("", "Unit"),
            scope: ("combustion, tank-to-wheel", ""),
            factor_id: "",
            geo: US,
        },
    },
    // ORR Table 6100 (ODS, Crown copyright, OGL v3): GB passenger rail, g CO2e per passenger-km
    // from traction energy, per financial year (April to March; the year is the first one).
    FactorMapping {
        dataset: "orr-6100",
        dataset_label: "ORR Table 6100: normalised passenger CO2e emissions, Great Britain",
        publisher: "Office of Rail and Road (ORR)",
        path_contains: &["table-6100-", ".ods"],
        format: Format::Ods {
            sheet: "6100_Passenger_CO2_emissions",
        },
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &["Time period", ORR_VALUE],
        keep: &[],
        value: Col::Name(ORR_VALUE),
        year: Year::FirstYearIn("Time period"),
        min_year: 0,
        skip_zero: false,
        max_bytes: 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "Passenger rail, traction energy",
            activity: &[],
            footnotes: false,
            categories: &[("", "rail")],
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2e",
            unit_mass: ("g CO2e", ""),
            unit_per: ("passenger.km", ""),
            scope: ("traction energy, April-March year", ""),
            factor_id: "",
            geo: Geo::Fixed("Great Britain", "GB"),
        },
    },
    // FuelEconomy.gov (EPA/DOE, US public domain): tailpipe g CO2 per mile of every US model,
    // model years from 2022.
    FactorMapping {
        dataset: "fueleconomy",
        dataset_label: "FuelEconomy.gov vehicle data (EPA/DOE), tailpipe CO2",
        publisher: "US EPA and US Department of Energy (FuelEconomy.gov)",
        path_contains: &["/feg/epadata/vehicles.csv.zip"],
        format: Format::ZipCsv {
            member: &["vehicles.csv"],
        },
        anchor: "",
        header_rows: 1,
        fill_down: &[],
        required: &["id", "year", "make", "model", "co2TailpipeGpm"],
        keep: &[],
        value: Col::Name("co2TailpipeGpm"),
        year: Year::Column("year"),
        min_year: 2022,
        skip_zero: false,
        max_bytes: 8 * 1024 * 1024,
        output: Output::Factor {
            activity_prefix: "",
            activity: &["make", "model", "VClass", "trany"],
            footnotes: false,
            categories: &[("", "car")],
            category_from: &[],
            gas_basis: &[],
            gas_default: "co2",
            unit_mass: ("g CO2", ""),
            unit_per: ("mile", ""),
            scope: ("tailpipe, model year", ""),
            factor_id: "id",
            geo: US,
        },
    },
    // The Big Climate Database v1.2 (CONCITO / 2-0 LCA, CC BY 4.0): one sheet per country,
    // kg CO2e per kg of food, cradle to retail. A 0 total means no figure for that country.
    concito(
        "concito-dk",
        "The Big Climate Database v1.2 (CONCITO / 2-0 LCA), Denmark",
        "DK",
        Geo::Fixed("Denmark", "DK"),
    ),
    concito(
        "concito-gb",
        "The Big Climate Database v1.2 (CONCITO / 2-0 LCA), United Kingdom",
        "GB",
        Geo::Fixed("United Kingdom", "GB"),
    ),
    concito(
        "concito-nl",
        "The Big Climate Database v1.2 (CONCITO / 2-0 LCA), Netherlands",
        "NL",
        Geo::Fixed("Netherlands", "NL"),
    ),
    concito(
        "concito-fr",
        "The Big Climate Database v1.2 (CONCITO / 2-0 LCA), France",
        "FR",
        Geo::Fixed("France", "FR"),
    ),
    concito(
        "concito-es",
        "The Big Climate Database v1.2 (CONCITO / 2-0 LCA), Spain",
        "ES",
        Geo::Fixed("Spain", "ES"),
    ),
    // RIVM Dutch food LCA database (CC BY 4.0), 2024 edition: kg CO2e per kg, to retail and to
    // consumption (incl. preparation and losses). RIVM: not critically reviewed, so compare
    // food groups, not single foods.
    rivm(
        "rivm-retail",
        "RIVM Database milieubelasting voedingsmiddelen, cradle to retail",
        "tot-en-met-retail",
        "cradle to retail (NL)",
    ),
    rivm(
        "rivm-consumption",
        "RIVM Database milieubelasting voedingsmiddelen, cradle to consumption",
        "tot-en-met-consumptie",
        "cradle to consumption (NL)",
    ),
    // UK DfT TAG data book (OGL v3), table A1.3.3.2: vehicle occupancies based on vehicle-km
    // travelled (persons per vehicle, driver included), by vehicle type and journey purpose,
    // all-week average. Only cars (each purpose and the average) and PSV (bus and coach) in
    // total: the PSV purpose rows split the total between purposes (they add up to it), so
    // they are not occupancies of a trip of that purpose; goods vehicles are not travel.
    FactorMapping {
        dataset: "dft-tag-occupancy",
        dataset_label: "DfT TAG data book, table A1.3.3.2 (vehicle occupancies)",
        publisher: "UK Department for Transport (DfT)",
        path_contains: &["tag-data-book", ".xlsm"],
        format: Format::Xlsx { sheet: "A1.3.3.2" },
        anchor: "",
        header_rows: 2,
        fill_down: &["Vehicle Type"],
        required: &["Journey Purpose", "Vehicle Type", "All Week"],
        keep: &[],
        value: Col::Name("All Week"),
        year: Year::Preamble("Vehicle occupancies ("),
        min_year: 0,
        skip_zero: true,
        max_bytes: 12 * 1024 * 1024,
        output: Output::Occupancy {
            label: &["Vehicle Type", "Journey Purpose"],
            rows: &[
                ("Car / Work", "car", "work"),
                ("Car / Commuting", "car", "commuting"),
                ("Car / Other", "car", "other"),
                ("Car / Average Car", "car", "all"),
                ("PSV / Total", "bus_coach", "all"),
            ],
            basis: "per_vehicle_km",
            includes_driver: true,
            time_period: "all_week",
            geo: Geo::Fixed("Great Britain", "GB"),
            as_of_from_preamble: true,
        },
    },
];

/// Every mapping that applies to a file row (by URL path), in [`MAPPINGS`] order.
pub fn mappings_for(url: &CanonicalUrl) -> Vec<&'static FactorMapping> {
    let p = url.as_url().path();
    MAPPINGS
        .iter()
        .filter(|m| m.path_contains.iter().all(|s| p.contains(s)))
        .collect()
}

/// The first mapping of a file row (by URL path).
pub fn mapping_for(url: &CanonicalUrl) -> Option<&'static FactorMapping> {
    mappings_for(url).into_iter().next()
}

/// A table: header and rows of trimmed cell texts, projected onto the columns the mapping
/// uses, plus the non-empty cells above the header.
#[derive(Debug, Default)]
pub struct Table {
    /// Header cells.
    pub header: Vec<String>,
    /// Data rows.
    pub rows: Vec<Vec<String>>,
    /// Non-empty cells above the header (titles, units), at most about 200.
    pub preamble: Vec<String>,
}

fn cell_text(c: &calamine::Data) -> String {
    use calamine::Data;
    match c {
        Data::Empty => String::new(),
        Data::String(s) => s.trim().to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 1e15 {
                format!("{}", *f as i64)
            } else {
                format!("{f}")
            }
        }
        Data::Int(i) => i.to_string(),
        Data::Bool(b) => b.to_string(),
        other => other.to_string().trim().to_string(),
    }
}

/// The header names a mapping reads (besides a value-column prefix).
fn used_columns(m: &FactorMapping) -> HashSet<&'static str> {
    let mut u: HashSet<&'static str> = m.required.iter().copied().collect();
    u.extend(m.keep.iter().map(|k| k.column));
    if let Col::Name(n) = m.value {
        u.insert(n);
    }
    if let Year::Column(c) | Year::FirstYearIn(c) = m.year {
        u.insert(c);
    }
    let geo = match m.output {
        Output::Factor {
            activity,
            category_from,
            unit_mass,
            unit_per,
            scope,
            factor_id,
            geo,
            ..
        } => {
            u.extend(activity.iter().copied());
            u.extend(category_from.iter().copied());
            u.extend([unit_mass.1, unit_per.1, scope.1, factor_id]);
            geo
        }
        Output::GridIntensity { geo, .. } => geo,
        Output::Occupancy { label, geo, .. } => {
            u.extend(label.iter().copied());
            geo
        }
    };
    u.extend(m.fill_down.iter().copied());
    match geo {
        Geo::Fixed(..) => {}
        Geo::CountryColumn { column, .. } => {
            u.insert(column);
        }
        Geo::Iso3 { code, name } | Geo::Region { code, name, .. } => {
            u.extend([code, name]);
        }
    }
    u.remove("");
    u
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Builds the mapping's table from rows of cells (any format).
fn table_from_rows(
    m: &FactorMapping,
    rows: impl Iterator<Item = Result<Vec<String>>>,
) -> Result<Table> {
    let has_required = |cells: &[String]| m.required.iter().all(|r| cells.iter().any(|c| c == r));
    let used = used_columns(m);
    let mut anchored = m.anchor.is_empty();
    let mut t = Table::default();
    let mut keep: Vec<usize> = Vec::new();
    let mut label_ix: Option<usize> = None;
    let mut prev: Vec<String> = Vec::new();
    let mut fill: Vec<(usize, String)> = Vec::new();
    for row in rows {
        let mut cells = row?;
        if !anchored {
            anchored = cells.iter().any(|c| c == m.anchor);
            continue;
        }
        if t.header.is_empty() {
            // Header cells are compared with their whitespace collapsed (a line break inside a
            // header cell is one space).
            let mut cells: Vec<String> = cells.iter().map(|c| collapse_ws(c)).collect();
            if m.header_rows == 2 && !has_required(&cells) {
                let joined = two_row_header(&prev, &cells);
                if has_required(&joined) {
                    cells = joined;
                }
            }
            prev.clone_from(&cells);
            if has_required(&cells) {
                fill = m
                    .fill_down
                    .iter()
                    .filter_map(|f| cells.iter().position(|c| c == f))
                    .map(|i| (i, String::new()))
                    .collect();
                keep = cells
                    .iter()
                    .enumerate()
                    .filter(|(_, h)| {
                        used.contains(h.as_str())
                            || matches!(m.value, Col::Prefix(p) if h.starts_with(p))
                    })
                    .map(|(i, _)| i)
                    .collect();
                label_ix = m
                    .required
                    .first()
                    .and_then(|r| cells.iter().position(|c| c == r));
                t.header = keep.iter().map(|i| cells[*i].clone()).collect();
            } else if t.preamble.len() < 200 {
                t.preamble
                    .extend(cells.into_iter().filter(|c| !c.is_empty()).take(50));
            }
            continue;
        }
        if !m.anchor.is_empty() {
            let label = label_ix
                .and_then(|i| cells.get(i))
                .map(String::as_str)
                .unwrap_or("");
            if label.is_empty() || label.starts_with("Source") || label.starts_with("Note") {
                break;
            }
        }
        for (i, last) in &mut fill {
            match cells.get(*i).map(String::as_str) {
                Some(c) if !c.is_empty() && !(c.starts_with('(') && c.ends_with(')')) => {
                    last.clone_from(&cells[*i]);
                }
                _ if cells.iter().any(|c| !c.is_empty()) && !last.is_empty() => {
                    if cells.len() <= *i {
                        cells.resize(*i + 1, String::new());
                    }
                    cells[*i].clone_from(last);
                }
                _ => {}
            }
        }
        if cells.iter().any(|c| !c.is_empty()) {
            t.rows.push(
                keep.iter()
                    .map(|i| cells.get(*i).cloned().unwrap_or_default())
                    .collect(),
            );
        }
    }
    if t.header.is_empty() {
        return Err(Error::Invalid(format!(
            "{ID}: {}: no header row with the required columns",
            m.dataset
        )));
    }
    Ok(t)
}

/// A header split over two rows, joined cell by cell (`"Weekday"` + `"7am – 10am"` →
/// `"Weekday 7am – 10am"`; an empty cell takes the other row's text).
fn two_row_header(above: &[String], row: &[String]) -> Vec<String> {
    (0..above.len().max(row.len()))
        .map(|i| {
            let a = above.get(i).map(String::as_str).unwrap_or("");
            let b = row.get(i).map(String::as_str).unwrap_or("");
            match (a.is_empty(), b.is_empty()) {
                (true, _) => b.to_string(),
                (false, true) => a.to_string(),
                (false, false) => format!("{a} {b}"),
            }
        })
        .collect()
}

fn csv_table(m: &FactorMapping, body: &[u8]) -> Result<Table> {
    let body = body.strip_prefix(b"\xef\xbb\xbf").unwrap_or(body);
    let mut r = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(body);
    table_from_rows(
        m,
        r.records().map(|rec| {
            rec.map(|rec| rec.iter().map(|c| c.trim().to_string()).collect())
                .map_err(|e| Error::Invalid(format!("{ID}: csv: {e}")))
        }),
    )
}

/// The chosen member of a ZIP body, decoded (bounded by [`MAX_UNZIPPED_BYTES`]).
pub fn unzip_member(member: &'static [&'static str], body: &[u8]) -> Result<Vec<u8>> {
    unzip_member_capped(member, body, MAX_UNZIPPED_BYTES)
}

/// Compressed bytes fed to the inflater per step of [`unzip_member`].
const UNZIP_CHUNK: usize = 1024;

/// [`unzip_member`] with an explicit cap on the decoded size.
#[doc(hidden)]
pub fn unzip_member_capped(
    member: &'static [&'static str],
    body: &[u8],
    cap: usize,
) -> Result<Vec<u8>> {
    let mut z = ZipStream::new(member);
    let mut out = Vec::new();
    // Small input chunks keep the overshoot past the cap small: deflate expands at most
    // ~1032:1, so one 1 KiB chunk adds about 1 MiB before the check.
    for chunk in body.chunks(UNZIP_CHUNK) {
        z.push(chunk, &mut out)?;
        if out.len() > cap {
            return Err(Error::Invalid(format!(
                "{ID}: zip member larger than {cap} bytes"
            )));
        }
    }
    z.finish()?;
    if out.is_empty() {
        return Err(Error::Invalid(format!("{ID}: zip: no {member:?} member")));
    }
    Ok(out)
}

/// Reads the mapping's table from a stored body. The header must hold every required column
/// (after the anchor, for a stacked table).
pub fn read_table(m: &FactorMapping, body: &[u8]) -> Result<Table> {
    use calamine::Reader;
    let range = match m.format {
        Format::Csv => return csv_table(m, body),
        Format::ZipCsv { member } => return csv_table(m, &unzip_member(member, body)?),
        Format::Xlsx { sheet } => calamine::Xlsx::new(Cursor::new(body))
            .map_err(|e| Error::Invalid(format!("{ID}: xlsx: {e}")))?
            .worksheet_range(sheet)
            .map_err(|e| Error::Invalid(format!("{ID}: xlsx sheet {sheet:?}: {e}")))?,
        Format::Ods { sheet } => calamine::Ods::new(Cursor::new(body))
            .map_err(|e| Error::Invalid(format!("{ID}: ods: {e}")))?
            .worksheet_range(sheet)
            .map_err(|e| Error::Invalid(format!("{ID}: ods sheet {sheet:?}: {e}")))?,
    };
    table_from_rows(
        m,
        range.rows().map(|r| Ok(r.iter().map(cell_text).collect())),
    )
}

/// A snake_case slug (ASCII letters and digits, `_` between words).
pub fn slug(label: &str) -> String {
    let folded = countries::fold(label);
    let mut out = String::new();
    for c in folded.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').chars().take(160).collect()
}

fn is_year(w: &[u8]) -> bool {
    w.len() == 4
        && w.iter().all(u8::is_ascii_digit)
        && (w.starts_with(b"19") || w.starts_with(b"20"))
}

/// The first 19xx/20xx year standing alone in a text (`"Apr 2024 to Mar 2025"` → 2024).
pub fn first_year(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    (0..b.len().saturating_sub(3)).find_map(|i| {
        let before = i == 0 || !b[i - 1].is_ascii_digit();
        let after = b.get(i + 4).is_none_or(|c| !c.is_ascii_digit());
        (before && after && is_year(&b[i..i + 4]))
            .then(|| s.get(i..i + 4)?.parse().ok())
            .flatten()
    })
}

const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

/// The first day of the month a text starts with, as `YYYY-MM-01` (`"May 2026 v2.03"` →
/// `2026-05-01`); `None` unless the text starts with an English month name and a year.
pub fn month_year(s: &str) -> Option<String> {
    let mut w = s.split_whitespace();
    let month = w.next()?;
    let m = MONTHS.iter().position(|n| n.eq_ignore_ascii_case(month))? + 1;
    let y = w.next()?;
    is_year(y.as_bytes()).then(|| format!("{y}-{m:02}-01"))
}

/// The last 19xx/20xx year in a URL's file name that is not followed by another digit
/// (`Version%201.2_DK_ENG_rev_18122025_0.xlsx` → 2025).
pub fn file_name_year(url: &CanonicalUrl) -> Option<i64> {
    let name = url.as_url().path_segments()?.next_back()?.to_string();
    let b = name.as_bytes();
    (0..b.len().saturating_sub(3)).rev().find_map(|i| {
        let after = b.get(i + 4).is_none_or(|c| !c.is_ascii_digit());
        (after && is_year(&b[i..i + 4]))
            .then(|| name.get(i..i + 4)?.parse().ok())
            .flatten()
    })
}

/// Drops a trailing footnote letter (`"Passenger Car A"` → `"Passenger Car"`).
pub fn strip_footnote(s: &str) -> &str {
    let s = s.trim();
    match s.rsplit_once(' ') {
        Some((head, last)) if last.len() == 1 && last.bytes().all(|c| c.is_ascii_uppercase()) => {
            head.trim_end()
        }
        _ => s,
    }
}

fn obj(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v.filter(|v| !v.is_null() && v.as_str() != Some("")) {
            m.insert(k.to_string(), v);
        }
    }
    Value::Object(m)
}

fn clip(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn region_code_ok(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 16
        && c.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// A mapped row: `(id, kind, data)`.
pub type FactorItem = (Option<String>, &'static str, Value);

/// The records of a stored file: every mapping of the file, in [`MAPPINGS`] order.
pub fn items(url: &CanonicalUrl, body: &[u8]) -> Result<Vec<FactorItem>> {
    let ms = mappings_for(url);
    if ms.is_empty() {
        return Err(Error::Invalid(format!("{ID}: no mapping for this file")));
    }
    let mut out = Vec::new();
    for m in ms {
        out.extend(mapping_items(m, url, body)?);
    }
    Ok(out)
}

fn mapping_items(m: &FactorMapping, url: &CanonicalUrl, body: &[u8]) -> Result<Vec<FactorItem>> {
    let t = read_table(m, body)?;
    let cols: HashMap<&str, usize> = t
        .header
        .iter()
        .enumerate()
        .rev()
        .map(|(i, h)| (h.as_str(), i))
        .collect();
    let idx = |c: &Col| -> Option<usize> {
        match c {
            Col::None => None,
            Col::Name(n) => cols.get(n).copied(),
            Col::Prefix(p) => t.header.iter().position(|h| h.starts_with(p)),
        }
    };
    let value_ix = idx(&m.value)
        .ok_or_else(|| Error::Invalid(format!("{ID}: {}: no value column", m.dataset)))?;
    let fixed_year = match m.year {
        Year::ValueHeaderSuffix => {
            let h = &t.header[value_ix];
            let y = h
                .get(h.len().saturating_sub(4)..)
                .and_then(|s| s.parse::<i64>().ok());
            Some(y.ok_or_else(|| {
                Error::Invalid(format!("{ID}: {}: no year in the value header", m.dataset))
            })?)
        }
        Year::Preamble(prefix) => {
            let y = t.preamble.iter().find_map(|c| {
                let i = c.find(prefix)? + prefix.len();
                c.get(i..i + 4)?.parse::<i64>().ok()
            });
            Some(y.ok_or_else(|| {
                Error::Invalid(format!(
                    "{ID}: {}: no {prefix}<year> above the header",
                    m.dataset
                ))
            })?)
        }
        Year::FileName => Some(file_name_year(url).ok_or_else(|| {
            Error::Invalid(format!("{ID}: {}: no year in the file name", m.dataset))
        })?),
        Year::Column(_) | Year::FirstYearIn(_) => None,
    };
    let get = |row: &[String], name: &str| -> String {
        if name.is_empty() {
            return String::new();
        }
        cols.get(name)
            .and_then(|i| row.get(*i))
            .cloned()
            .unwrap_or_default()
    };
    let join = |row: &[String], names: &[&str], footnotes: bool| -> String {
        names
            .iter()
            .map(|n| {
                let v = get(row, n);
                if footnotes {
                    strip_footnote(&v).to_string()
                } else {
                    v
                }
            })
            .filter(|v| !v.is_empty())
            .collect::<Vec<_>>()
            .join(" / ")
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    'rows: for row in &t.rows {
        for k in m.keep {
            if !k.values.contains(&get(row, k.column).as_str()) {
                continue 'rows;
            }
        }
        let Some(value) = row
            .get(value_ix)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
        else {
            continue; // blank = no factor published (never 0)
        };
        if m.skip_zero && value == 0.0 {
            continue;
        }
        let year = match (fixed_year, m.year) {
            (Some(y), _) => y,
            (None, Year::Column(c)) => match get(row, c).parse::<i64>() {
                Ok(y) => y,
                Err(_) => continue,
            },
            (None, Year::FirstYearIn(c)) => match first_year(&get(row, c)) {
                Some(y) => y,
                None => continue,
            },
            (None, _) => continue,
        };
        if year < m.min_year {
            continue;
        }
        match m.output {
            Output::Factor {
                activity_prefix,
                activity,
                footnotes,
                categories,
                category_from,
                gas_basis,
                gas_default,
                unit_mass,
                unit_per,
                scope,
                factor_id,
                geo,
            } => {
                let cells = join(row, activity, footnotes);
                let label = match (activity_prefix.is_empty(), cells.is_empty()) {
                    (true, _) => cells,
                    (false, true) => activity_prefix.to_string(),
                    (false, false) => format!("{activity_prefix} / {cells}"),
                };
                if label.is_empty() {
                    continue;
                }
                let cat_key = join(row, category_from, false);
                let Some(category) = categories
                    .iter()
                    .find(|(k, _)| cat_key.contains(k))
                    .map(|(_, c)| *c)
                else {
                    continue;
                };
                let gas = gas_basis
                    .iter()
                    .find(|(k, _)| label.contains(k))
                    .map(|(_, g)| *g)
                    .unwrap_or(gas_default);
                let pick = |(fixed, col): (&str, &str)| {
                    let v = get(row, col);
                    if v.is_empty() { fixed.to_string() } else { v }
                };
                let unit = format!("{} per {}", pick(unit_mass), pick(unit_per));
                let mut region = String::new();
                let (geography, code) = match geo {
                    Geo::Fixed(n, c) => (n.to_string(), c.to_string()),
                    Geo::CountryColumn {
                        column,
                        when,
                        fallback,
                    } if when.contains(&category) => {
                        let name = get(row, column);
                        if name.starts_with("UK") {
                            (fallback.0.to_string(), fallback.1.to_string())
                        } else {
                            let c = countries::iso2(&name).unwrap_or("");
                            (name, c.to_string())
                        }
                    }
                    Geo::CountryColumn { fallback, .. } => {
                        (fallback.0.to_string(), fallback.1.to_string())
                    }
                    Geo::Iso3 { code, name } => {
                        let Some(cc) = iso3::alpha2(&get(row, code)) else {
                            continue; // regions, aggregates
                        };
                        let country = countries::name(cc)
                            .map(str::to_string)
                            .unwrap_or_else(|| get(row, name));
                        (country, cc.to_string())
                    }
                    Geo::Region {
                        code,
                        name,
                        label: what,
                        country,
                        national,
                        emit_national,
                        own_country,
                    } => {
                        let rc = get(row, code);
                        let (cc, total) = match own_country.iter().find(|o| o.0 == rc) {
                            Some(&(_, cc, cname, true)) => {
                                (cc, Some((cname.to_string(), cc.to_string())))
                            }
                            Some(&(_, cc, _, false)) => (cc, None),
                            None => (country.1, None),
                        };
                        if let Some(t) = total {
                            t
                        } else if rc == national {
                            if !emit_national {
                                continue;
                            }
                            (country.0.to_string(), country.1.to_string())
                        } else if region_code_ok(&rc) {
                            let n = get(row, name);
                            let g = if n.is_empty() {
                                format!("{what} {rc}")
                            } else {
                                format!("{n} ({what} {rc})")
                            };
                            region = rc;
                            (g, cc.to_string())
                        } else {
                            continue;
                        }
                    }
                };
                let act = slug(&label);
                if act.is_empty() {
                    continue;
                }
                let fid = get(row, factor_id);
                let key = match (fid.is_empty(), matches!(geo, Geo::Iso3 { .. })) {
                    (false, _) => fid.clone(),
                    (true, true) => format!("{code}:{act}"),
                    (true, false) => act.clone(),
                };
                let id = format!("{}:{year}:{key}", m.dataset);
                if !seen.insert(id.clone()) {
                    continue;
                }
                let data = obj(vec![
                    ("activity", Some(json!(act))),
                    ("activity_label", Some(json!(clip(&label, 300)))),
                    ("category", Some(json!(category))),
                    ("geography", Some(json!(clip(&geography, 80)))),
                    ("country_code", (!code.is_empty()).then(|| json!(code))),
                    ("region_code", (!region.is_empty()).then(|| json!(region))),
                    ("unit", Some(json!(clip(&unit, 80)))),
                    ("value", Some(json!(value))),
                    ("gas_basis", Some(json!(gas))),
                    ("scope", Some(json!(clip(&pick(scope), 40)))),
                    ("year", Some(json!(year))),
                    ("publisher", Some(json!(m.publisher))),
                    (
                        "dataset",
                        Some(json!(clip(&format!("{} {year}", m.dataset_label), 160))),
                    ),
                    ("factor_id", Some(json!(clip(&fid, 80)))),
                ]);
                out.push((Some(id), "emission_factor", data));
            }
            Output::GridIntensity { geo, basis } => {
                let Geo::Iso3 { code, name } = geo else {
                    continue;
                };
                let Some(cc) = iso3::alpha2(&get(row, code)) else {
                    continue; // regions, aggregates
                };
                let country = countries::name(cc)
                    .map(str::to_string)
                    .unwrap_or_else(|| get(row, name));
                let data = obj(vec![
                    ("country", Some(json!(clip(&country, 80)))),
                    ("country_code", Some(json!(cc))),
                    ("period", Some(json!("annual"))),
                    ("year", Some(json!(year))),
                    ("date", Some(json!(format!("{year:04}-01-01")))),
                    ("intensity_g_per_kwh", Some(json!(value))),
                    ("basis", Some(json!(basis))),
                    ("publisher", Some(json!(m.publisher))),
                    ("dataset", Some(json!(m.dataset_label))),
                ]);
                out.push((
                    Some(format!("{}:{cc}:{year}", m.dataset)),
                    "grid_carbon_intensity",
                    data,
                ));
            }
            Output::Occupancy {
                label,
                rows,
                basis,
                includes_driver,
                time_period,
                geo,
                as_of_from_preamble,
            } => {
                let Geo::Fixed(geography, code) = geo else {
                    continue;
                };
                let row_label = join(row, label, false);
                let Some(&(_, mode, purpose)) = rows.iter().find(|r| r.0 == row_label) else {
                    continue;
                };
                let id = format!("{}:{year}:{}", m.dataset, slug(&row_label));
                if !seen.insert(id.clone()) {
                    continue;
                }
                let as_of = as_of_from_preamble
                    .then(|| t.preamble.iter().find_map(|c| month_year(c)))
                    .flatten();
                let data = obj(vec![
                    ("mode", Some(json!(mode))),
                    ("mode_label", Some(json!(clip(&row_label, 160)))),
                    ("trip_purpose", Some(json!(purpose))),
                    ("time_period", Some(json!(time_period))),
                    ("geography", Some(json!(geography))),
                    ("country_code", Some(json!(code))),
                    ("value", Some(json!(value))),
                    ("unit", Some(json!("persons per vehicle"))),
                    ("basis", Some(json!(basis))),
                    ("includes_driver", Some(json!(includes_driver))),
                    ("year", Some(json!(year))),
                    ("publisher", Some(json!(m.publisher))),
                    ("dataset", Some(json!(clip(m.dataset_label, 160)))),
                    ("as_of", as_of.map(|d| json!(d))),
                ]);
                out.push((Some(id), "occupancy", data));
            }
        }
    }
    Ok(out)
}

/// The adapter.
#[derive(Debug, Clone, Copy)]
pub struct FactorTable;

#[async_trait]
impl SourceAdapter for FactorTable {
    fn id(&self) -> &'static str {
        ID
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        let ms = mappings_for(&url);
        let first = *ms
            .first()
            .ok_or_else(|| Error::Invalid(format!("{ID}: no mapping for this row")))?;
        let mut rep = AdapterReport::default();
        let opts = Fetch {
            hint: Some(match first.format {
                Format::Csv => MediaType::Csv,
                _ => MediaType::Other,
            }),
            conditional: true,
            max_body_bytes: ms
                .iter()
                .map(|m| m.max_bytes)
                .max()
                .unwrap_or(first.max_bytes),
            ..Fetch::default()
        };
        let ex = fetch_raw(ctx, url, &opts, &mut rep).await?;
        if !not_modified(&ex) {
            check_status(&ex)?;
            for m in &ms {
                let t = read_table(m, &ex.body)?;
                tracing::info!(
                    adapter = ID,
                    dataset = m.dataset,
                    rows = t.rows.len(),
                    "parsed"
                );
            }
        }
        rep.complete = true;
        Ok(rep)
    }
}
