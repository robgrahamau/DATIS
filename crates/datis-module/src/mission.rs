use std::collections::HashMap;
use std::str::FromStr;

use datis_core::rpc::*;
use datis_core::station::*;
use datis_core::tts::TextToSpeechProvider;
use hlua51::{Lua, LuaFunction, LuaTable};
use rand::Rng;
use regex::{Regex, RegexBuilder};

pub struct Info {
    pub stations: Vec<Station>,
    pub gcloud_key: String,
    pub aws_key: String,
    pub aws_secret: String,
    pub aws_region: String,
    pub srs_port: u16,
    pub executable_path: String,
    pub rpc: MissionRpc,
}

pub fn extract(mut lua: Lua<'static>) -> Result<Info, anyhow::Error> {
    debug!("Extracting ATIS stations from Mission Situation");

    let default_voice = {
        // OptionsData.getPlugin("DATIS", "defaultVoice")
        let mut options_data: LuaTable<_> = get!(lua, "OptionsData")?;
        let mut get_plugin: LuaFunction<_> = get!(options_data, "getPlugin")?;

        let default_voice: String = get_plugin
            .call_with_args(("DATIS", "defaultVoice"))
            .map_err(|_| new_lua_call_error("getPlugin"))?;

        default_voice
    };

    // read gcloud access key option
    let (gcloud_key, aws_key, aws_secret, aws_region) = {
        // OptionsData.getPlugin("DATIS", "gcloudAccessKey")
        let mut options_data: LuaTable<_> = get!(lua, "OptionsData")?;
        let mut get_plugin: LuaFunction<_> = get!(options_data, "getPlugin")?;

        let gcloud_key: String = get_plugin
            .call_with_args(("DATIS", "gcloudAccessKey"))
            .map_err(|_| new_lua_call_error("getPlugin"))?;

        let aws_key: String = get_plugin
            .call_with_args(("DATIS", "awsAccessKey"))
            .map_err(|_| new_lua_call_error("getPlugin"))?;

        let aws_secret: String = get_plugin
            .call_with_args(("DATIS", "awsPrivateKey"))
            .map_err(|_| new_lua_call_error("getPlugin"))?;

        let aws_region: String = get_plugin
            .call_with_args(("DATIS", "awsRegion"))
            .map_err(|_| new_lua_call_error("getPlugin"))?;

        (gcloud_key, aws_key, aws_secret, aws_region)
    };

    // read srs server port
    let srs_port = {
        // OptionsData.getPlugin("DATIS", "srsPort")
        let mut options_data: LuaTable<_> = get!(lua, "OptionsData")?;
        let mut get_plugin: LuaFunction<_> = get!(options_data, "getPlugin")?;

        let port: u16 = get_plugin
            .call_with_args(("DATIS", "srsPort"))
            .map_err(|_| new_lua_call_error("getPlugin"))?;
        info!("Using SRS Server port: {}", port);
        port
    };

    // read write dir: lfs.writedir()
    let writedir = {
        let mut lfs: LuaTable<_> = get!(lua, "lfs")?;

        let mut get_writedir: LuaFunction<_> = get!(lfs, "writedir")?;
        let writedir: String = get_writedir.call()?;
        writedir
    };

    // extract frequencies from mission briefing, which is retrieved from
    // `DCS.getMissionDescription()`
    let frequencies = {
        let mut dcs: LuaTable<_> = get!(lua, "DCS")?;

        let mut get_mission_description: LuaFunction<_> = get!(dcs, "getMissionDescription")?;
        let mission_situation: String = get_mission_description.call()?;

        extract_atis_station_frequencies(&mission_situation)
    };

    // Create a random generator for creating the information letter offset.
    let mut rng = rand::thread_rng();

    // collect all airfields on the current loaded terrain
    let mut airfields = {
        let mut airfields = HashMap::new();

        // read `Terrain.GetTerrainConfig('Airdromes')`
        let mut terrain: LuaTable<_> = get!(lua, "Terrain")?;
        let mut get_terrain_config: LuaFunction<_> = get!(terrain, "GetTerrainConfig")?;
        let mut airdromes: LuaTable<_> = get_terrain_config
            .call_with_args("Airdromes")
            .map_err(|_| new_lua_call_error("GetTerrainConfig"))?;

        // on Caucasus, airdromes start at the index 12, others start at 1; also hlua's table
        // iterator does not work for tables of tables, which is why we are just iterating
        // from 1 to 50 an check whether there is an airdrome table at this index or not
        for i in 1..=50 {
            if let Some(mut airdrome) = airdromes.get::<LuaTable<_>, _, _>(i) {
                let display_name: String = get!(airdrome, "display_name")?;

                let (x, y) = {
                    let mut reference_point: LuaTable<_> = get!(airdrome, "reference_point")?;
                    let x: f64 = get!(reference_point, "x")?;
                    let y: f64 = get!(reference_point, "y")?;
                    (x, y)
                };

                let mut runways: Vec<String> = Vec::new();
                let mut rwys: LuaTable<_> = get!(airdrome, "runways")?;
                let mut j = 0;
                while let Some(mut rw) = rwys.get::<LuaTable<_>, _, _>(j) {
                    j += 1;
                    let start: String = get!(rw, "start")?;
                    let end: String = get!(rw, "end")?;
                    runways.push(start);
                    runways.push(end);
                }

                airfields.insert(
                    display_name.clone(),
                    Airfield {
                        name: display_name,
                        position: Position { x, y, alt: 0.0 },
                        runways,
                        traffic_freq: None,
                        info_ltr_offset: rng.gen_range(0, 25),
                    },
                );
            }
        }

        airfields
    };

    // extract all mission statics and ship units to later look for ATIS configs in their names
    let mut mission_units = {
        let mut current_mission: LuaTable<_> = get!(lua, "_current_mission")?;
        let mut mission: LuaTable<_> = get!(current_mission, "mission")?;
        let mut coalitions: LuaTable<_> = get!(mission, "coalition")?;

        let mut mission_units = Vec::new();

        let keys = vec!["blue", "red"];
        for key in keys {
            let mut coalition: LuaTable<_> = get!(coalitions, key)?;
            let mut countries: LuaTable<_> = get!(coalition, "country")?;

            let mut i = 1;
            while let Some(mut country) = countries.get::<LuaTable<_>, _, _>(i) {
                // `_current_mission.mission.coalition.{blue,red}.country[i].{static|plane|helicopter|vehicle|ship}.group[j]
                let keys = vec!["static", "plane", "helicopter", "vehicle", "ship"];
                for key in keys {
                    if let Some(mut assets) = country.get::<LuaTable<_>, _, _>(key) {
                        if let Some(mut groups) = assets.get::<LuaTable<_>, _, _>("group") {
                            let mut j = 1;
                            while let Some(mut group) = groups.get::<LuaTable<_>, _, _>(j) {
                                if let Some(mut units) = group.get::<LuaTable<_>, _, _>("units") {
                                    let mut k = 1;
                                    while let Some(mut unit) = units.get::<LuaTable<_>, _, _>(k) {
                                        let x: f64 = get!(unit, "x")?;
                                        let y: f64 = get!(unit, "y")?;
                                        let alt: Option<f64> = get!(unit, "alt").ok();
                                        let unit_id: u32 = get!(unit, "unitId")?;

                                        mission_units.push(MissionUnit {
                                            id: unit_id,
                                            name: String::new(),
                                            x,
                                            y,
                                            alt: alt.unwrap_or(0.0),
                                        });

                                        k += 1;
                                    }
                                }

                                j += 1;
                            }
                        }
                    }
                }

                i += 1;
            }
        }

        mission_units
    };

    // extract the names for all units
    {
        // read `DCS.getUnitProperty`
        let mut dcs: LuaTable<_> = get!(lua, "DCS")?;
        let mut get_unit_property: LuaFunction<_> = get!(dcs, "getUnitProperty")?;

        for mut unit in &mut mission_units {
            // 3 = DCS.UNIT_NAME
            unit.name = get_unit_property
                .call_with_args((unit.id, 3))
                .map_err(|_| new_lua_call_error("getUnitProperty"))?;
        }
    }

    // read the terrain height for all airdromes and units
    {
        // read `Terrain.GetHeight`
        let mut terrain: LuaTable<_> = get!(lua, "Terrain")?;
        let mut get_height: LuaFunction<_> = get!(terrain, "GetHeight")?;

        for mut airfield in airfields.values_mut() {
            airfield.position.alt = get_height
                .call_with_args((airfield.position.x, airfield.position.y))
                .map_err(|_| new_lua_call_error("getHeight"))?;
        }

        for mut unit in &mut mission_units {
            if unit.alt == 0.0 {
                unit.alt = get_height
                    .call_with_args((unit.x, unit.y))
                    .map_err(|_| new_lua_call_error("getHeight"))?;
            }
        }
    }

    // extract the current mission's weather kind and static weather configuration
    let (clouds, fog_thickness, fog_visibility) = {
        // read `_current_mission.mission.weather`
        let mut current_mission: LuaTable<_> = get!(lua, "_current_mission")?;
        let mut mission: LuaTable<_> = get!(current_mission, "mission")?;
        let mut weather: LuaTable<_> = get!(mission, "weather")?;

        // read `_current_mission.mission.weather.atmosphere_type`
        let atmosphere_type: f64 = get!(weather, "atmosphere_type")?;
        let is_dynamic = atmosphere_type != 0.0;

        let clouds = {
            if is_dynamic {
                None
            } else {
                let mut clouds: LuaTable<_> = get!(weather, "clouds")?;
                Some(Clouds {
                    base: get!(clouds, "base")?,
                    density: get!(clouds, "density")?,
                    thickness: get!(clouds, "thickness")?,
                    iprecptns: get!(clouds, "iprecptns")?,
                })
            }
        };

        // Note: `weather.visibility` is always the same, which is why we cannot use it here
        // and use the fog instead to derive some kind of visibility

        let mut fog: LuaTable<_> = get!(weather, "fog")?;
        let fog_thickness: u32 = get!(fog, "thickness")?;
        let fog_visibility: u32 = get!(fog, "visibility")?;

        (clouds, fog_thickness, fog_visibility)
    };

    // YOLO initialize the atmosphere, because DCS initializes it only after hitting the
    // "Briefing" button, which is something most of the time not done for "dedicated" servers
    {
        lua.execute::<()>(
            r#"
            local Weather = require 'Weather'
            Weather.initAtmospere(_current_mission.mission.weather)
        "#,
        )?;
    }

    // initialize the dynamic weather component
    let rpc = MissionRpc::new(clouds, fog_thickness, fog_visibility)?;

    let default_voice = match TextToSpeechProvider::from_str(&default_voice) {
        Ok(default_voice) => default_voice,
        Err(err) => {
            warn!("Invalid default voice `{}`: {}", default_voice, err);
            TextToSpeechProvider::default()
        }
    };

    // combine the frequencies that have extracted from the mission's situation with their
    // corresponding airfield
    let mut stations: Vec<Station> = frequencies
        .into_iter()
        .filter_map(|(name, freq)| {
            airfields.remove(&name).map(|airfield| Station {
                name,
                freq: freq.atis,
                tts: default_voice.clone(),
                transmitter: Transmitter::Airfield(airfield),
                rpc: Some(rpc.clone()),
            })
        })
        .collect();

    // check all units if they represent and ATIS station and if so, combine them with
    // their corresponding airfield
    stations.extend(mission_units.iter().filter_map(|mission_unit| {
        extract_atis_station_config(&mission_unit.name).and_then(|config| {
            airfields.remove(&config.name).map(|mut airfield| {
                airfield.traffic_freq = config.traffic;
                airfield.position.x = mission_unit.x;
                airfield.position.y = mission_unit.y;
                airfield.position.alt = mission_unit.alt;

                Station {
                    name: config.name,
                    freq: config.atis,
                    tts: config.tts.unwrap_or_else(|| default_voice.clone()),
                    transmitter: Transmitter::Airfield(airfield),
                    rpc: Some(rpc.clone()),
                }
            })
        })
    }));

    if stations.is_empty() {
        info!("No ATIS stations found ...");
    } else {
        info!("ATIS Stations:");
        for station in &stations {
            info!(
                "  - {} (Freq: {}, Voice: {:?})",
                station.name, station.freq, station.tts
            );
        }
    }

    let carriers = mission_units
        .iter()
        .filter_map(|mission_unit| {
            extract_carrier_station_config(&mission_unit.name).map(|config| Station {
                name: config.name.clone(),
                freq: config.atis,
                tts: config.tts.unwrap_or_else(|| default_voice.clone()),
                transmitter: Transmitter::Carrier(Carrier {
                    name: config.name,
                    unit_id: mission_unit.id,
                    unit_name: mission_unit.name.clone(),
                }),
                rpc: Some(rpc.clone()),
            })
        })
        .collect::<Vec<_>>();

    if carriers.is_empty() {
        info!("No Carrier stations found ...");
    } else {
        info!("Carrier Stations:");
        for station in &carriers {
            info!(
                "  - {} (Freq: {}, Voice: {:?})",
                station.name, station.freq, station.tts
            );
        }
    }

    let broadcasts = mission_units
        .iter()
        .filter_map(|mission_unit| {
            extract_custom_broadcast_config(&mission_unit.name).map(|config| Station {
                name: mission_unit.name.clone(),
                freq: config.freq,
                tts: config.tts.unwrap_or_else(|| default_voice.clone()),
                transmitter: Transmitter::Custom(Custom {
                    unit_id: mission_unit.id,
                    unit_name: mission_unit.name.clone(),
                    message: config.message,
                }),
                rpc: Some(rpc.clone()),
            })
        })
        .collect::<Vec<_>>();

    if broadcasts.is_empty() {
        info!("No custom Broadcast stations found ...");
    } else {
        info!("Broadcast Stations:");
        for station in &broadcasts {
            info!(
                "  - {} (Freq: {}, Voice: {:?})",
                station.name, station.freq, station.tts
            );
        }
    }

    let weather_stations = mission_units
        .iter()
        .filter_map(|mission_unit| {
            extract_weather_station_config(&mission_unit.name).map(|config| Station {
                name: mission_unit.name.clone(),
                freq: config.freq,
                tts: config.tts.unwrap_or_else(|| default_voice.clone()),
                transmitter: Transmitter::Weather(WeatherTransmitter {
                    name: config.name,
                    unit_id: mission_unit.id,
                    unit_name: mission_unit.name.clone(),
                    info_ltr_offset: rng.gen_range(0, 25),
                }),
                rpc: Some(rpc.clone()),
            })
        })
        .collect::<Vec<_>>();

    if weather_stations.is_empty() {
        info!("No weather stations found ...");
    } else {
        info!("Weather Stations:");
        for station in &weather_stations {
            info!(
                "  - {} (Freq: {}, Voice: {:?})",
                station.name, station.freq, station.tts
            );
        }
    }

    stations.extend(carriers);
    stations.extend(broadcasts);
    stations.extend(weather_stations);

    Ok(Info {
        stations,
        gcloud_key,
        aws_key,
        aws_secret,
        aws_region,
        srs_port,
        executable_path: format!("{}Mods\\tech\\DATIS\\bin\\", writedir),
        rpc,
    })
}

fn new_lua_call_error(method_name: &str) -> anyhow::Error {
    anyhow!("failed to call lua function {}", method_name)
}

#[derive(Debug)]
struct MissionUnit {
    id: u32,
    name: String,
    x: f64,
    y: f64,
    alt: f64,
}

#[derive(Debug, PartialEq)]
struct StationConfig {
    name: String,
    atis: u64,
    traffic: Option<u64>,
    tts: Option<TextToSpeechProvider>,
}

fn extract_atis_station_frequencies(situation: &str) -> HashMap<String, StationConfig> {
    // extract ATIS stations and frequencies
    let re = Regex::new(r"ATIS ([a-zA-Z- ]+) ([1-3]\d{2}(\.\d{1,3})?)").unwrap();
    let mut stations: HashMap<String, StationConfig> = re
        .captures_iter(situation)
        .map(|caps| {
            let name = caps.get(1).unwrap().as_str().to_string();
            let freq = caps.get(2).unwrap().as_str();
            let freq = (f32::from_str(freq).unwrap() * 1_000_000.0) as u64;
            (
                name.clone(),
                StationConfig {
                    name,
                    atis: freq,
                    traffic: None,
                    tts: None,
                },
            )
        })
        .collect();

    // extract optional traffic frequencies
    let re = Regex::new(r"TRAFFIC ([a-zA-Z-]+) ([1-3]\d{2}(\.\d{1,3})?)").unwrap();
    for caps in re.captures_iter(situation) {
        let name = caps.get(1).unwrap().as_str();
        let freq = caps.get(2).unwrap().as_str();
        let freq = (f32::from_str(freq).unwrap() * 1_000_000.0) as u64;

        if let Some(freqs) = stations.get_mut(name) {
            freqs.traffic = Some(freq);
        }
    }

    stations
}

fn extract_atis_station_config(config: &str) -> Option<StationConfig> {
    let re = RegexBuilder::new(
        r"^ATIS ([a-zA-Z- ]+) ([1-3]\d{2}(\.\d{1,3})?)(,[ ]?TRAFFIC ([1-3]\d{2}(\.\d{1,3})?))?(,[ ]?VOICE ([a-zA-Z-:]+))?$",
    )
    .case_insensitive(true)
    .build()
    .unwrap();
    re.captures(config).map(|caps| {
        let name = caps.get(1).unwrap().as_str();
        let atis_freq = caps.get(2).unwrap().as_str();
        let atis_freq = (f64::from_str(atis_freq).unwrap() * 1_000_000.0) as u64;
        let traffic_freq = caps
            .get(5)
            .map(|freq| (f64::from_str(freq.as_str()).unwrap() * 1_000_000.0) as u64);
        let tts = caps
            .get(8)
            .and_then(|s| TextToSpeechProvider::from_str(s.as_str()).ok());
        StationConfig {
            name: name.to_string(),
            atis: atis_freq,
            traffic: traffic_freq,
            tts,
        }
    })
}

fn extract_carrier_station_config(config: &str) -> Option<StationConfig> {
    let re = RegexBuilder::new(
        r"^CARRIER ([a-zA-Z- ]+) ([1-3]\d{2}(\.\d{1,3})?)(,[ ]?VOICE ([a-zA-Z-:]+))?$",
    )
    .case_insensitive(true)
    .build()
    .unwrap();
    re.captures(config).map(|caps| {
        let name = caps.get(1).unwrap().as_str();
        let atis_freq = caps.get(2).unwrap().as_str();
        let atis_freq = (f64::from_str(atis_freq).unwrap() * 1_000_000.0) as u64;
        let tts = caps
            .get(5)
            .and_then(|s| TextToSpeechProvider::from_str(s.as_str()).ok());
        StationConfig {
            name: name.to_string(),
            atis: atis_freq,
            traffic: None,
            tts,
        }
    })
}

#[derive(Debug, PartialEq)]
struct BroadcastConfig {
    freq: u64,
    message: String,
    tts: Option<TextToSpeechProvider>,
}

fn extract_custom_broadcast_config(config: &str) -> Option<BroadcastConfig> {
    let re = RegexBuilder::new(
        r"^BROADCAST ([1-3]\d{2}(\.\d{1,3})?)(,[ ]?VOICE ([a-zA-Z-:]+))?:[ ]*(.+)$",
    )
    .case_insensitive(true)
    .build()
    .unwrap();
    re.captures(config).map(|caps| {
        let freq = caps.get(1).unwrap().as_str();
        let freq = (f32::from_str(freq).unwrap() * 1_000_000.0) as u64;
        let tts = caps
            .get(4)
            .and_then(|s| TextToSpeechProvider::from_str(s.as_str()).ok());
        let message = caps.get(5).unwrap().as_str();
        BroadcastConfig {
            freq,
            message: message.to_string(),
            tts,
        }
    })
}

#[derive(Debug, PartialEq)]
struct WetherStationConfig {
    name: String,
    freq: u64,
    tts: Option<TextToSpeechProvider>,
}

fn extract_weather_station_config(config: &str) -> Option<WetherStationConfig> {
    let re = RegexBuilder::new(
        r"^WEATHER ([a-zA-Z- ]+) ([1-3]\d{2}(\.\d{1,3})?)(,[ ]?VOICE ([a-zA-Z-:]+))?$",
    )
    .case_insensitive(true)
    .build()
    .unwrap();
    re.captures(config).map(|caps| {
        let name = caps.get(1).unwrap().as_str();
        let freq = caps.get(2).unwrap().as_str();
        let freq = (f64::from_str(freq).unwrap() * 1_000_000.0) as u64;
        let tts = caps
            .get(5)
            .and_then(|s| TextToSpeechProvider::from_str(s.as_str()).ok());
        WetherStationConfig {
            name: name.to_string(),
            freq,
            tts,
        }
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use datis_core::tts::{aws, gcloud, TextToSpeechProvider};

    #[test]
    fn test_mission_situation_extraction() {
        let freqs = extract_atis_station_frequencies(
            r#"
            ATIS Mineralnye Vody 251.000
            ATIS Batumi 131.5
            ATIS Senaki-Kolkhi 145

            TRAFFIC Batumi 255.00
        "#,
        );

        assert_eq!(
            freqs,
            vec![
                (
                    "Mineralnye Vody".to_string(),
                    StationConfig {
                        name: "Mineralnye Vody".to_string(),
                        atis: 251_000_000,
                        traffic: None,
                        tts: None,
                    }
                ),
                (
                    "Batumi".to_string(),
                    StationConfig {
                        name: "Batumi".to_string(),
                        atis: 131_500_000,
                        traffic: Some(255_000_000),
                        tts: None,
                    }
                ),
                (
                    "Senaki-Kolkhi".to_string(),
                    StationConfig {
                        name: "Senaki-Kolkhi".to_string(),
                        atis: 145_000_000,
                        traffic: None,
                        tts: None,
                    }
                )
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn test_atis_config_extraction() {
        assert_eq!(
            extract_atis_station_config("ATIS Kutaisi 251"),
            Some(StationConfig {
                name: "Kutaisi".to_string(),
                atis: 251_000_000,
                traffic: None,
                tts: None,
            })
        );

        assert_eq!(
            extract_atis_station_config("ATIS Mineralnye Vody 251"),
            Some(StationConfig {
                name: "Mineralnye Vody".to_string(),
                atis: 251_000_000,
                traffic: None,
                tts: None,
            })
        );

        assert_eq!(
            extract_atis_station_config("ATIS Senaki-Kolkhi 251"),
            Some(StationConfig {
                name: "Senaki-Kolkhi".to_string(),
                atis: 251_000_000,
                traffic: None,
                tts: None,
            })
        );

        assert_eq!(
            extract_atis_station_config("ATIS Kutaisi 251.000, TRAFFIC 123.45"),
            Some(StationConfig {
                name: "Kutaisi".to_string(),
                atis: 251_000_000,
                traffic: Some(123_450_000),
                tts: None,
            })
        );

        assert_eq!(
            extract_atis_station_config(
                "ATIS Kutaisi 251.000, TRAFFIC 123.45, VOICE en-US-Standard-E"
            ),
            Some(StationConfig {
                name: "Kutaisi".to_string(),
                atis: 251_000_000,
                traffic: Some(123_450_000),
                tts: Some(TextToSpeechProvider::GoogleCloud {
                    voice: gcloud::VoiceKind::StandardE
                }),
            })
        );

        assert_eq!(
            extract_atis_station_config("ATIS Kutaisi 251.000, VOICE en-US-Standard-E"),
            Some(StationConfig {
                name: "Kutaisi".to_string(),
                atis: 251_000_000,
                traffic: None,
                tts: Some(TextToSpeechProvider::GoogleCloud {
                    voice: gcloud::VoiceKind::StandardE
                }),
            })
        );

        assert_eq!(
            extract_atis_station_config("ATIS Kutaisi 131.400"),
            Some(StationConfig {
                name: "Kutaisi".to_string(),
                atis: 131_400_000,
                traffic: None,
                tts: None,
            })
        );
    }

    #[test]
    fn test_carrier_config_extraction() {
        assert_eq!(
            extract_carrier_station_config("CARRIER Mother 251"),
            Some(StationConfig {
                name: "Mother".to_string(),
                atis: 251_000_000,
                traffic: None,
                tts: None,
            })
        );

        assert_eq!(
            extract_carrier_station_config("CARRIER Mother 131.400"),
            Some(StationConfig {
                name: "Mother".to_string(),
                atis: 131_400_000,
                traffic: None,
                tts: None,
            })
        );

        assert_eq!(
            extract_carrier_station_config("CARRIER Mother 251.000, VOICE en-US-Standard-E"),
            Some(StationConfig {
                name: "Mother".to_string(),
                atis: 251_000_000,
                traffic: None,
                tts: Some(TextToSpeechProvider::GoogleCloud {
                    voice: gcloud::VoiceKind::StandardE
                }),
            })
        );
    }

    #[test]
    fn test_cloud_provider_prefix_extraction() {
        assert_eq!(
            extract_atis_station_config("ATIS Kutaisi 131.400, VOICE GC:en-US-Standard-D"),
            Some(StationConfig {
                name: "Kutaisi".to_string(),
                atis: 131_400_000,
                traffic: None,
                tts: Some(TextToSpeechProvider::GoogleCloud {
                    voice: gcloud::VoiceKind::StandardD
                }),
            })
        );

        assert_eq!(
            extract_atis_station_config("ATIS Kutaisi 131.400, VOICE AWS:Brian"),
            Some(StationConfig {
                name: "Kutaisi".to_string(),
                atis: 131_400_000,
                traffic: None,
                tts: Some(TextToSpeechProvider::AmazonWebServices {
                    voice: aws::VoiceKind::Brian
                }),
            })
        );
    }

    #[test]
    fn test_broadcast_config_extraction() {
        assert_eq!(
            extract_custom_broadcast_config("BROADCAST 251: Bla bla"),
            Some(BroadcastConfig {
                freq: 251_000_000,
                message: "Bla bla".to_string(),
                tts: None,
            })
        );

        assert_eq!(
            extract_custom_broadcast_config("BROADCAST 251.000, VOICE AWS:Brian: Bla bla"),
            Some(BroadcastConfig {
                freq: 251_000_000,
                message: "Bla bla".to_string(),
                tts: Some(TextToSpeechProvider::AmazonWebServices {
                    voice: aws::VoiceKind::Brian
                }),
            })
        );
    }

    #[test]
    fn test_weather_station_config_extraction() {
        assert_eq!(
            extract_weather_station_config("WEATHER Shooting Range 251"),
            Some(WetherStationConfig {
                name: "Shooting Range".to_string(),
                freq: 251_000_000,
                tts: None,
            })
        );

        assert_eq!(
            extract_weather_station_config("WEATHER Coast 131.400"),
            Some(WetherStationConfig {
                name: "Coast".to_string(),
                freq: 131_400_000,
                tts: None,
            })
        );

        assert_eq!(
            extract_weather_station_config(
                "WEATHER Mountain Range 251.000, VOICE en-US-Standard-E"
            ),
            Some(WetherStationConfig {
                name: "Mountain Range".to_string(),
                freq: 251_000_000,
                tts: Some(TextToSpeechProvider::GoogleCloud {
                    voice: gcloud::VoiceKind::StandardE
                }),
            })
        );
    }
}
