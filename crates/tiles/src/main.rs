use tiles_toolbox::{mosaic, pyramid, stacta, tiles};

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use pyramid::Resampling;
use tiles::{Format, Scheme};

#[derive(Parser)]
#[command(version, about = "本地 Web Mercator 地图切片工具")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 拼接一个级别，输出 RGBA GeoTIFF
    Mosaic {
        input: PathBuf,
        #[arg(long, value_enum, default_value = "xyz")]
        scheme: Scheme,
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=30))]
        zoom: u8,
        #[arg(long)]
        output: PathBuf,
    },
    /// 从同一个源级别生成新的切片金字塔
    Pyramid {
        input: PathBuf,
        #[arg(long, value_enum, default_value = "xyz")]
        scheme: Scheme,
        #[arg(long, value_parser = clap::value_parser!(u8).range(0..=30))]
        source_zoom: u8,
        #[arg(long)]
        zooms: String,
        #[arg(long, value_enum)]
        resampling: Option<Resampling>,
        #[arg(long, value_enum, default_value = "png")]
        format: Format,
        #[arg(long, default_value = "#FFFFFF", value_parser = pyramid::parse_background)]
        background: [u8; 3],
        #[arg(long)]
        output: PathBuf,
    },
    /// 生成 STACTA 索引，或将 TMS 导出为 XYZ 并生成索引
    Stacta {
        input: PathBuf,
        #[arg(long, value_enum, default_value = "xyz")]
        scheme: Scheme,
        #[arg(long, required_if_eq("scheme", "tms"))]
        output: Option<PathBuf>,
        #[arg(long, value_parser = chrono::DateTime::parse_from_rfc3339)]
        datetime: chrono::DateTime<chrono::FixedOffset>,
        #[arg(long)]
        id: Option<String>,
    },
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Mosaic {
            input,
            scheme,
            zoom,
            output,
        } => {
            let index = tiles::Index::scan(&input, scheme)?;
            mosaic::run(index.level(zoom)?, zoom, &output)
        }
        Command::Pyramid {
            input,
            scheme,
            source_zoom,
            zooms,
            resampling,
            format,
            background,
            output,
        } => {
            let zooms = pyramid::parse_zooms(&zooms)?;
            let index = tiles::Index::scan(&input, scheme)?;
            let options = pyramid::Options {
                scheme,
                source_zoom,
                zooms,
                resampling,
                format,
                background,
            };
            pyramid::run(index.level(source_zoom)?, &options, &output)
        }
        Command::Stacta {
            input,
            scheme,
            output,
            datetime,
            id,
        } => stacta::run(&input, scheme, output.as_deref(), datetime, id),
    }
}
