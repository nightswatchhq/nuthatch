// #889: what does a sealed segment actually contain? Read the footer rather than infer from crate
// defaults, because "the default is probably X" is how a measurement becomes an assumption.
use parquet::file::reader::{FileReader, SerializedFileReader};
use std::fs::File;

fn main() {
    for path in std::env::args().skip(1) {
        let f = File::open(&path).expect("open");
        let r = SerializedFileReader::new(f).expect("parse footer");
        let md = r.metadata();
        let fmd = md.file_metadata();
        println!("── {} ──", path.rsplit('/').next().unwrap_or(&path));
        println!(
            "  rows: {}  row_groups: {}",
            fmd.num_rows(),
            md.num_row_groups()
        );
        println!("  writer: {}", fmd.created_by().unwrap_or("?"));
        for (i, rg) in md.row_groups().iter().enumerate() {
            println!(
                "  rg{i}: rows={} bytes={}",
                rg.num_rows(),
                rg.total_byte_size()
            );
            let sort = match rg.sorting_columns() {
                None => "no sort metadata".to_string(),
                Some(cols) => format!("sort {cols:?}"),
            };
            println!("    {sort}");
            for c in rg.columns() {
                let stats = if c.statistics().is_some() {
                    "stats"
                } else {
                    "NO stats"
                };
                let bloom = if c.bloom_filter_offset().is_some() {
                    "bloom"
                } else {
                    "NO bloom"
                };
                let dict = if c.dictionary_page_offset().is_some() {
                    "dict"
                } else {
                    "NO dict"
                };
                let codec = format!("{:?}", c.compression());
                println!(
                    "     {:<28} {codec:<8} {stats:<9} {bloom:<9} {dict}",
                    c.column_path().string()
                );
            }
        }
    }
}
