#![deny(
    missing_debug_implementations,
    trivial_numeric_casts,
    unstable_features,
    unused_import_braces,
    unused_qualifications
)]

extern crate clap;
extern crate notify;
extern crate rouille;
extern crate tempdir;
extern crate hyper;
extern crate hyper_rustls;
extern crate pbr;
extern crate app_dirs;
extern crate libflate;
extern crate tar;
extern crate sha1;
extern crate sha2;
extern crate digest;
extern crate toml;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate cargo_shim;
extern crate handlebars;
extern crate unicode_categories;
extern crate ordermap;

extern crate parity_wasm;
#[macro_use]
extern crate log;
extern crate rustc_demangle;

use std::process::{Command, Stdio, exit};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::sync::{Mutex, Arc};
use std::time::Duration;
use std::thread;
use std::io::{self, Read, Write};
use std::net::{self, ToSocketAddrs};
use std::fs;
use std::time::Instant;
use std::error;
use std::fmt;
use std::env;

use notify::{
    RecommendedWatcher,
    Watcher,
    RecursiveMode,
    DebouncedEvent
};

use clap::{
    Arg,
    App,
    AppSettings,
    SubCommand
};

use tempdir::TempDir;

use hyper::Client;
use hyper::header::{Connection, ContentLength};
use hyper::net::HttpConnector;
use hyper::net::HttpsConnector;
use hyper::client::ProxyConfig;
use hyper::Url;

use handlebars::Handlebars;

use libflate::gzip;

use digest::{Input, Digest};

use cargo_shim::*;

#[macro_use]
mod utils;
mod config;
mod wasm;
mod wasm_gc;
mod wasm_inline_js;
mod wasm_export_main;
mod wasm_export_table;
mod wasm_hook_grow;
mod wasm_runtime;
mod wasm_context;

use utils::*;
use config::Config;

const APP_INFO: app_dirs::AppInfo = app_dirs::AppInfo {name: "cargo-web", author: "Jan Bujak"};

const DEFAULT_INDEX_HTML: &'static str = r#"
<!DOCTYPE html>
<head>
    <meta charset="utf-8" />
    <meta http-equiv="X-UA-Compatible" content="IE=edge" />
    <meta content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=1" name="viewport" />
    <script>
        var Module = {};
        var __cargo_web = {};
        Object.defineProperty( Module, 'canvas', {
            get: function() {
                if( __cargo_web.canvas ) {
                    return __cargo_web.canvas;
                }

                var canvas = document.createElement( 'canvas' );
                document.querySelector( 'body' ).appendChild( canvas );
                __cargo_web.canvas = canvas;

                return canvas;
            }
        });
    </script>
</head>
<body>
    <script src="js/app.js"></script>
</body>
</html>
"#;

const DEFAULT_TEST_INDEX_HTML: &'static str = r#"
<!DOCTYPE html>
<head>
    <meta charset="utf-8" />
    <meta http-equiv="X-UA-Compatible" content="IE=edge" />
    <meta content="width=device-width, initial-scale=1.0, maximum-scale=1.0, user-scalable=1" name="viewport" />
    <script>
        var __cargo_web = {};
        __cargo_web.print_counter = 0;
        __cargo_web.xhr_queue = [];
        __cargo_web.xhr_in_progress = 0;
        __cargo_web.flush_xhr = function() {
            if( __cargo_web.xhr_queue.length === 0 ) {
                return;
            }
            var next_callback = __cargo_web.xhr_queue.shift();
            next_callback();
        };
        __cargo_web.send_xhr = function( url, data ) {
            var callback = function() {
                __cargo_web.xhr_in_progress++;
                var xhr = new XMLHttpRequest();
                xhr.open( 'PUT', url );
                xhr.setRequestHeader( 'Content-Type', 'text/plain' );
                xhr.onload = function() {
                    __cargo_web.xhr_in_progress--;
                    __cargo_web.flush_xhr();
                };
                xhr.send( data );
            };
            __cargo_web.xhr_queue.push( callback );
            if( __cargo_web.xhr_in_progress === 0 ) {
                __cargo_web.flush_xhr();
            }
        };
        __cargo_web.print = function( message ) {
            __cargo_web.print_counter++;
            if( (__cargo_web.print_counter === 1 && /pre-main prep time/.test( message )) ||
                (__cargo_web.print_counter === 2 && message === '') ) {
                return;
            }

            __cargo_web.send_xhr( '/__cargo_web/print', message );
        };
        __cargo_web.on_exit = function( status ) {
            __cargo_web.send_xhr( '/__cargo_web/exit', status );
        };
        var Module = {};
        Module['print'] = __cargo_web.print;
        Module['printErr'] = __cargo_web.print;
        Module['onExit'] = __cargo_web.on_exit;
        Module['arguments'] = [{{#each arguments}} "{{{ this }}}", {{/each}}];
    </script>
</head>
<body>
    <script src="js/app.js"></script>
</body>
</html>
"#;

struct Output {
    path: PathBuf,
    data: Vec< u8 >
}

impl AsRef< Path > for Output {
    fn as_ref( &self ) -> &Path {
        &self.path
    }
}

impl Output {
    fn has_extension( &self, extension: &str ) -> bool {
        self.path.extension().map( |ext| ext == extension ).unwrap_or( false )
    }

    fn is_js( &self ) -> bool {
        self.has_extension( "js" )
    }

    fn is_wasm( &self ) -> bool {
        self.has_extension( "wasm" )
    }
}

fn monitor_for_changes_and_rebuild(
    package: &CargoPackage,
    target: &CargoTarget,
    build: BuildConfig,
    extra_path: Option< &Path >,
    outputs: Arc< Mutex< Vec< Output > > >
) -> RecommendedWatcher {
    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher = Watcher::new( tx, Duration::from_millis( 500 ) ).unwrap();

    // TODO: Support local dependencies.
    // TODO: Support Cargo.toml reloading.
    watcher.watch( &target.source_directory, RecursiveMode::Recursive ).unwrap();
    watcher.watch( &package.manifest_path, RecursiveMode::NonRecursive ).unwrap();

    let extra_path = extra_path.map( |path| path.to_owned() );
    thread::spawn( move || {
        let rx = rx;
        while let Ok( event ) = rx.recv() {
            match event {
                DebouncedEvent::Create( _ ) |
                DebouncedEvent::Remove( _ ) |
                DebouncedEvent::Rename( _, _ ) |
                DebouncedEvent::Write( _ ) => {},
                _ => continue
            };

            println_err!( "==== Triggering `cargo build` ====" );

            let mut command = build.as_command();
            if let Some( ref extra_path ) = extra_path {
                command.append_to_path( extra_path );
            }

            if command.run().is_ok() {
                let mut outputs = outputs.lock().unwrap();
                wasm::process_wasm_files( &build, &outputs );

                for output in outputs.iter_mut() {
                    if let Ok( data ) = read_bytes( &output.path ) {
                        output.data = data;
                    }
                }
            }
        }
    });

    watcher
}

fn check_if_command_exists( command: &str, extra_path: Option< &str > ) -> bool {
    let mut command = Command::new( command );
    command.arg( "--version" );
    if let Some( extra_path ) = extra_path {
        command.append_to_path( extra_path );
    }

    command
        .stdout( Stdio::null() )
        .stderr( Stdio::null() )
        .stdin( Stdio::null() );

    return command.spawn().is_ok()
}

fn unpack< I: AsRef< Path >, O: AsRef< Path > >( input_path: I, output_path: O ) -> Result< (), Box< io::Error > > {
    let output_path = output_path.as_ref();
    let file = fs::File::open( input_path )?;
    let decoder = gzip::Decoder::new( file )?;
    let mut archive = tar::Archive::new( decoder );
    archive.unpack( output_path )?;

    Ok(())
}

struct PrebuiltPackage {
    url: &'static str,
    name: &'static str,
    version: &'static str,
    arch: &'static str,
    hash: &'static str,
    size: u64,
}

fn emscripten_package() -> Option< PrebuiltPackage > {
    let package =
        if cfg!( target_os = "linux" ) && cfg!( target_arch = "x86_64" ) {
            PrebuiltPackage {
                url: "https://github.com/koute/emscripten-build/releases/download/emscripten-1.37.26-1/emscripten-1.37.26-1-x86_64-unknown-linux-gnu.tgz",
                name: "emscripten",
                version: "1.37.26-1",
                arch: "x86_64-unknown-linux-gnu",
                hash: "0b8392bf6b22f13b99bfedeff2d0d1eae2bbd876e796f9b01468179facd66a00",
                size: 136903726
            }
        } else if cfg!( target_os = "linux" ) && cfg!( target_arch = "x86" ) {
            PrebuiltPackage {
                url: "https://github.com/koute/emscripten-build/releases/download/emscripten-1.37.26-1/emscripten-1.37.26-1-i686-unknown-linux-gnu.tgz",
                name: "emscripten",
                version: "1.37.26-1",
                arch: "i686-unknown-linux-gnu",
                hash: "3cfe8c59812fb9bc2c61c21ce18158811af36dbb31229c567d3832b7b5e51f8b",
                size: 144521448
            }
        } else {
            return None;
        };

    Some( package )
}

fn binaryen_package() -> Option< PrebuiltPackage > {
    let package =
        if cfg!( target_os = "linux" ) && cfg!( target_arch = "x86_64" ) {
            PrebuiltPackage {
                url: "https://github.com/koute/emscripten-build/releases/download/emscripten-1.37.26-1/binaryen-1.37.26-1-x86_64-unknown-linux-gnu.tgz",
                name: "binaryen",
                version: "1.37.26-1",
                arch: "x86_64-unknown-linux-gnu",
                hash: "9192a71b05abff031ec14574d51e40744c332a9307142b9279eb3544344b3cee",
                size: 12625187
            }
        } else if cfg!( target_os = "linux" ) && cfg!( target_arch = "x86" ) {
            PrebuiltPackage {
                url: "https://github.com/koute/emscripten-build/releases/download/emscripten-1.37.26-1/binaryen-1.37.26-1-i686-unknown-linux-gnu.tgz",
                name: "binaryen",
                version: "1.37.26-1",
                arch: "i686-unknown-linux-gnu",
                hash: "3b09c8d55308ae1dfd2e369b6a0dad361b53db0d39ae592020b353ffaf84f260",
                size: 12706588
            }
        } else {
            return None;
        };

    Some( package )
}

fn download_package( package: &PrebuiltPackage ) -> PathBuf {
    let url = Url::parse( package.url ).unwrap();
    let package_filename = url.path_segments().unwrap().last().unwrap().to_owned();

    let unpack_path = app_dirs::app_dir( app_dirs::AppDataType::UserData, &APP_INFO, package.name )
        .unwrap()
        .join( package.arch );
    let version_path = unpack_path.join( ".version" );

    if let Ok( existing_version ) = read( &version_path ) {
        if existing_version == package.version {
            return unpack_path;
        }
    }

    if fs::metadata( &unpack_path ).is_ok() {
        fs::remove_dir_all( &unpack_path ).unwrap();
    }

    fs::create_dir_all( &unpack_path ).unwrap();

    let tls = hyper_rustls::TlsClient::new();
    let client = match env::var( "HTTP_PROXY" ) {
        Ok( proxy ) => {
            let proxy = match Url::parse( proxy.as_str() ) {
                Ok( url ) => url,
                Err( error ) => {
                    println_err!( "Invalid HTTP_PROXY: #{:?}", error );
                    exit( 101 );
                }
            };

            let connector = HttpConnector::default();
            let proxy_config = ProxyConfig::new(
                proxy.scheme(),
                proxy.host_str().unwrap().to_string(),
                proxy.port_or_known_default().unwrap(),
                connector,
                tls
            );
            Client::with_proxy_config( proxy_config )
        },
        _ => {
            let connector = HttpsConnector::new( tls );
            Client::with_connector( connector )
        }
    };

    println_err!( "Downloading {}...", package_filename );
    let mut response = client.get( url )
        .header( Connection::close() )
        .send().unwrap();

    let tmpdir = TempDir::new( format!( "cargo-web-{}-download", package.name ).as_str() ).unwrap();
    let dlpath = tmpdir.path().join( &package_filename );
    let mut fp = fs::File::create( &dlpath ).unwrap();

    let length: Option< ContentLength > = response.headers.get().cloned();
    let length = length.map( |length| length.0 ).unwrap_or( package.size );
    let mut pb = pbr::ProgressBar::new( length );
    pb.set_units( pbr::Units::Bytes );

    let mut buffer = Vec::new();
    buffer.resize( 1024 * 1024, 0 );

    let mut hasher = sha2::Sha256::default();
    loop {
        let length = match response.read( &mut buffer ) {
            Ok( 0 ) => break,
            Ok( length ) => length,
            Err( ref err ) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err( err ) => panic!( err )
        };

        let slice = &buffer[ 0..length ];
        hasher.digest( slice );
        fp.write_all( slice ).unwrap();
        pb.add( length as u64 );
    }

    pb.finish();

    let actual_hash = hasher.result();
    let actual_hash = actual_hash.map( |byte| format!( "{:02x}", byte ) ).join( "" );

    if actual_hash != package.hash {
        println_err!( "error: the hash of {} doesn't match the expected hash!", package_filename );
        println_err!( "  actual: {}", actual_hash );
        println_err!( "  expected: {}", package.hash );
        exit( 101 );
    }

    println_err!( "Unpacking {}...", package_filename );
    unpack( &dlpath, &unpack_path ).unwrap();
    write( &version_path, package.version ).unwrap();

    println_err!( "Package {} was successfully installed!", package_filename );
    return unpack_path;
}

fn check_for_emcc( use_system_emscripten: bool, targeting_webasm: bool ) -> Option< PathBuf > {
    let emscripten_package =
        if use_system_emscripten {
            None
        } else {
            emscripten_package()
        };

    let binaryen_package =
        if use_system_emscripten || !targeting_webasm {
            None
        } else {
            binaryen_package()
        };

    if let Some( package ) = binaryen_package {
        let binaryen_path = download_package( &package );
        env::set_var( "BINARYEN", &binaryen_path.join( "binaryen" ) );
    }

    if let Some( package ) = emscripten_package {
        let emscripten_path = download_package( &package );
        let emscripten_bin_path = emscripten_path.join( "emscripten" );
        let emscripten_llvm_path = emscripten_path.join( "emscripten-fastcomp" );

        env::set_var( "EMSCRIPTEN", &emscripten_bin_path );
        env::set_var( "EMSCRIPTEN_FASTCOMP", &emscripten_llvm_path );
        env::set_var( "LLVM", &emscripten_llvm_path );

        return Some( emscripten_bin_path );
    }

    if check_if_command_exists( "emcc", None ) {
        return None;
    }

    if cfg!( any(linux) ) && Path::new( "/usr/lib/emscripten/emcc" ).exists() {
        if check_if_command_exists( "emcc", Some( "/usr/lib/emscripten" ) ) {
            // Arch package doesn't put Emscripten anywhere in the $PATH, but
            // it's there and it works.
            return Some( "/usr/lib/emscripten".into() );
        }
    } else if cfg!( any(windows) ) {
        if check_if_command_exists( "emcc.bat", None ) {
            return None;
        }
    }

    println_err!( "error: you don't have Emscripten installed!" );
    println_err!( "" );

    if Path::new( "/usr/bin/pacman" ).exists() {
        println_err!( "You can most likely install it like this:" );
        println_err!( "  sudo pacman -S emscripten" );
    } else if Path::new( "/usr/bin/apt-get" ).exists() {
        println_err!( "You can most likely install it like this:" );
        println_err!( "  sudo apt-get install emscripten" );
    } else if cfg!( target_os = "linux" ) {
        println_err!( "You can most likely find it in your distro's repositories." );
    } else if cfg!( target_os = "windows" ) {
        println_err!( "Download and install emscripten from the official site: http://kripken.github.io/emscripten-site/docs/getting_started/downloads.html" );
    }

    if cfg!( unix ) {
        if cfg!( target_os = "linux" ) {
            println_err!( "If not you can install it manually like this:" );
        } else {
            println_err!( "You can install it manually like this:" );
        }
        println_err!( "  curl -O https://s3.amazonaws.com/mozilla-games/emscripten/releases/emsdk-portable.tar.gz" );
        println_err!( "  tar -xzf emsdk-portable.tar.gz" );
        println_err!( "  source emsdk_portable/emsdk_env.sh" );
        println_err!( "  emsdk update" );
        println_err!( "  emsdk install sdk-incoming-64bit" );
        println_err!( "  emsdk activate sdk-incoming-64bit" );
    }

    exit( 101 );
}

#[derive(Debug)]
enum Error {
    ConfigurationError( String ),
    EnvironmentError( String ),
    BuildError
}

impl error::Error for Error {
    fn description( &self ) -> &str {
        match *self {
            Error::ConfigurationError( ref message ) => &message,
            Error::EnvironmentError( ref message ) => &message,
            Error::BuildError => "build failed"
        }
    }
}

impl fmt::Display for Error {
    fn fmt( &self, formatter: &mut fmt::Formatter ) -> fmt::Result {
        use error::Error;
        write!( formatter, "{}", self.description() )
    }
}

struct BuildArgsMatcher< 'a > {
    matches: &'a clap::ArgMatches< 'a >,
    project: &'a CargoProject
}

impl< 'a > BuildArgsMatcher< 'a > {
    fn build_type( &self ) -> BuildType {
        let build_type = if self.matches.is_present( "release" ) {
            BuildType::Release
        } else {
            BuildType::Debug
        };

        if self.matches.is_present( "target-webasm" ) && build_type == BuildType::Debug {
            // TODO: Remove this in the future.
            println_err!( "warning: debug builds on the wasm-unknown-unknown are currently totally broken" );
            println_err!( "         forcing a release build" );
            return BuildType::Release;
        }

        build_type
    }

    fn package( &self ) -> Result< Option< &CargoPackage >, Error > {
        if let Some( name ) = self.matches.value_of( "package" ) {
            match self.project.packages.iter().find( |package| package.name == name ) {
                None => Err( Error::ConfigurationError( format!( "package `{}` not found", name ) ) ),
                package => Ok( package )
            }
        } else {
            Ok( None )
        }
    }

    fn package_or_default( &self ) -> Result< &CargoPackage, Error > {
        Ok( self.package()?.unwrap_or_else( || self.project.default_package() ) )
    }

    fn target( &'a self, package: &'a CargoPackage ) -> Result< Option< &'a CargoTarget >, Error > {
        let targets = &package.targets;
        if self.matches.is_present( "lib" ) {
            match targets.iter().find( |target| target.kind == TargetKind::Lib ) {
                None => return Err( Error::ConfigurationError( format!( "no library targets found" ) ) ),
                target => Ok( target )
            }
        } else if let Some( name ) = self.matches.value_of( "bin" ) {
            match targets.iter().find( |target| target.kind == TargetKind::Bin && target.name == name ) {
                None => return Err( Error::ConfigurationError( format!( "no bin target named `{}`", name ) ) ),
                target => Ok( target )
            }
        } else if let Some( name ) = self.matches.value_of( "example" ) {
            match targets.iter().find( |target| target.kind == TargetKind::Example && target.name == name ) {
                None => return Err( Error::ConfigurationError( format!( "no example target named `{}`", name ) ) ),
                target => Ok( target )
            }
        } else if let Some( name ) = self.matches.value_of( "bench" ) {
            match targets.iter().find( |target| target.kind == TargetKind::Bench && target.name == name ) {
                None => return Err( Error::ConfigurationError( format!( "no bench target named `{}`", name ) ) ),
                target => Ok( target )
            }
        } else {
            Ok( None )
        }
    }

    fn target_or_select< F >( &'a self, package: &'a CargoPackage, filter: F ) -> Result< Vec< &'a CargoTarget >, Error >
        where for< 'r > F: Fn( &'r CargoTarget ) -> bool
    {
        Ok( self.target( package )?.map( |target| vec![ target ] ).unwrap_or_else( || {
            package.targets.iter().filter( |target| filter( target ) ).collect()
        }))
    }

    fn triplet_or_default( &self ) -> &str {
        if self.matches.is_present( "target-webasm") {
            "wasm32-unknown-unknown"
        } else if self.matches.is_present( "target-webasm-emscripten" ) {
            "wasm32-unknown-emscripten"
        } else {
            "asmjs-unknown-emscripten"
        }
    }

    fn build_config( &self, package: &CargoPackage, target: &CargoTarget, profile: Profile ) -> BuildConfig {
        BuildConfig {
            build_target: target_to_build_target( target, profile ),
            build_type: self.build_type(),
            triplet: Some( self.triplet_or_default().into() ),
            package: Some( package.name.clone() )
        }
    }
}

fn address_or_default< 'a >( matches: &clap::ArgMatches< 'a > ) -> net::SocketAddr {
    let host = matches.value_of( "host" ).unwrap_or( "localhost" );
    let port = matches.value_of( "port" ).unwrap_or( "8000" );
    format!( "{}:{}", host, port ).to_socket_addrs().unwrap().next().unwrap()
}


fn run_with_broken_first_build_hack( package: &CargoPackage, build_config: &BuildConfig, command: &mut Command ) -> Result< (), Error > {
    if command.run().is_ok() == false {
        return Err( Error::BuildError );
    }

    let artifacts = build_config.potential_artifacts( &package.crate_root );
    wasm::process_wasm_files( build_config, &artifacts );

    // HACK: For some reason when you install emscripten for the first time
    // the first build is always a dud (it produces no artifacts), so we do this.
    if artifacts.is_empty() {
        if command.run().is_ok() == false {
            return Err( Error::BuildError );
        }
    }

    Ok(())
}

fn set_link_args( config: &Config ) {
    if let Some( ref link_args ) = config.link_args {
        let mut rustflags = String::new();
        if let Ok( flags ) = env::var( "RUSTFLAGS" ) {
            rustflags.push_str( flags.as_str() );
            rustflags.push_str( " " );
        }

        for arg in link_args {
            if arg.contains( " " ) {
                // Not sure how to handle spaces, as `-C link-arg="{}"` doesn't work.
                println_err!( "error: you have a space in one of the entries in `link-args` in your `Web.toml`;" );
                println_err!( "       this is currently unsupported - aborting!" );
                exit( 101 );
            }
            rustflags.push_str( format!( "-C link-arg={} ", arg ).as_str() );
        }

        env::set_var( "RUSTFLAGS", rustflags.trim() );
    }
}

fn command_build< 'a >( matches: &clap::ArgMatches< 'a >, project: &CargoProject ) -> Result< (), Error > {
    let use_system_emscripten = matches.is_present( "use-system-emscripten" );
    let targeting_webasm = matches.is_present( "target-webasm-emscripten" ) || matches.is_present( "target-webasm" );
    let extra_path = if matches.is_present( "target-webasm" ) { None } else { check_for_emcc( use_system_emscripten, targeting_webasm ) };

    let build_matcher = BuildArgsMatcher {
        matches: matches,
        project: project
    };

    let package = build_matcher.package_or_default()?;
    let config = Config::load_for_package_printing_warnings( &package ).unwrap().unwrap_or_default();
    set_link_args( &config );

    let targets = build_matcher.target_or_select( package, |target| {
        target.kind == TargetKind::Lib || target.kind == TargetKind::Bin
    })?;

    for target in targets {
        let build_config = build_matcher.build_config( package, target, Profile::Main );
        let mut command = build_config.as_command();
        if let Some( ref extra_path ) = extra_path {
            command.append_to_path( extra_path );
        }

        run_with_broken_first_build_hack( package, &build_config, &mut command )?;
    }

    Ok(())
}

fn command_test< 'a >( matches: &clap::ArgMatches< 'a >, project: &CargoProject ) -> Result< (), Error > {
    let use_nodejs = matches.is_present( "nodejs" );
    let use_system_emscripten = matches.is_present( "use-system-emscripten" );
    let targeting_webasm = matches.is_present( "target-webasm-emscripten" );
    let targeting_native_webasm = matches.is_present( "target-webasm" );
    let extra_path = if targeting_native_webasm {
        if !use_nodejs {
            return Err( Error::ConfigurationError( "running tests for the native wasm target is currently only supported with `--nodejs`".into() ) );
        }

        None
    } else {
        check_for_emcc( use_system_emscripten, targeting_webasm )
    };

    let no_run = matches.is_present( "no-run" );
    let arg_passthrough = matches.values_of_os( "passthrough" )
        .map_or(vec![], |args| args.collect());

    let mut chromium_executable = "";
    if !use_nodejs {
        chromium_executable = if cfg!( any(windows) ) && check_if_command_exists( "chrome.exe", None ) {
            "chrome.exe"
        } else if check_if_command_exists( "chromium", None ) {
            "chromium"
        } else if check_if_command_exists( "google-chrome", None ) {
            "google-chrome"
        } else if check_if_command_exists( "google-chrome-stable", None ) {
            "google-chrome-stable"
        } else {
            return Err( Error::EnvironmentError( "you need to have either Chromium or Chrome installed and in your PATH to run the tests!".into() ) );
        }
    }

    let build_matcher = BuildArgsMatcher {
        matches: matches,
        project: project
    };

    let package = build_matcher.package_or_default()?;
    let config = Config::load_for_package_printing_warnings( &package ).unwrap().unwrap_or_default();
    set_link_args( &config );

    let targets = build_matcher.target_or_select( package, |target| {
        target.kind == TargetKind::Lib || target.kind == TargetKind::Bin || target.kind == TargetKind::Test
    })?;

    let builds: Vec< _ > = targets.iter().map( |target| {
        let build_config = build_matcher.build_config( package, target, Profile::Test );
        let artifacts: Vec< _ > = build_config.potential_artifacts( &package.crate_root ).into_iter().map( |artifact| {
            let modified = fs::metadata( &artifact ).unwrap().modified().unwrap();
            (artifact, modified)
        }).collect();

        (build_config, artifacts)
    }).collect();

    let mut post_artifacts_per_build = Vec::new();
    for &(ref build_config, ref pre_artifacts) in &builds {
        let mut command = build_config.as_command();
        if let Some( ref extra_path ) = extra_path {
            command.append_to_path( extra_path );
        }

        run_with_broken_first_build_hack( package, &build_config, &mut command )?;

        let mut post_artifacts = build_config.potential_artifacts( &package.crate_root );

        let artifact =
        if post_artifacts.len() == 1 {
            post_artifacts.pop().unwrap()
        } else if post_artifacts.is_empty() {
            panic!( "internal error: post_artifacts are empty; please report this!" );
        } else {
            let mut new_artifacts = Vec::new();
            let mut modified_artifacts = Vec::new();

            for post_artifact in post_artifacts {
                if let Some( &(_, pre_modified) ) = pre_artifacts.iter().find( |&&(ref pre_artifact, _)| *pre_artifact == post_artifact ) {
                    let post_modified = fs::metadata( &post_artifact ).unwrap().modified().unwrap();
                    if post_modified > pre_modified {
                        modified_artifacts.push( post_artifact );
                    }
                } else {
                    new_artifacts.push( post_artifact );
                }
            }

            fn is_js( artifact: &PathBuf ) -> bool {
                artifact.extension().map( |ext| ext == "js" ).unwrap_or( false )
            }
            let mut new_artifacts: Vec< _ > = new_artifacts.into_iter().filter( is_js ).collect();
            let mut modified_artifacts: Vec< _ > = modified_artifacts.into_iter().filter( is_js ).collect();

            if new_artifacts.len() == 1 {
                new_artifacts.pop().unwrap()
            } else if new_artifacts.len() > 1 {
                panic!( "internal error: new_artifacts have {} elements; please report this!", new_artifacts.len() );
            } else if modified_artifacts.len() == 1 {
                modified_artifacts.pop().unwrap()
            } else if modified_artifacts.len() > 1 {
                panic!( "internal error: modified_artifacts have {} elements; please report this!", new_artifacts.len() );
            } else {
                panic!( "internal error: nothing changed so I don't know which artifact corresponds to this build; change something and try again!" );
            }
        };

        post_artifacts_per_build.push( artifact );
    }

    if no_run {
        exit( 0 );
    }

    let mut any_failure = false;
    if use_nodejs {
        for artifact in &post_artifacts_per_build {
            let nodejs_name =
                if cfg!( any(windows) ) && check_if_command_exists( "node.exe", None ) {
                    "node.exe"
                } else if check_if_command_exists( "nodejs", None ) {
                    "nodejs"
                } else if check_if_command_exists( "node", None ) {
                    "node"
                } else {
                    return Err( Error::EnvironmentError( "node.js not found; please install it!".into() ) );
                };

            let test_args = std::iter::once( artifact.as_os_str() )
               .chain( arg_passthrough.iter().cloned() );

            let previous_cwd = if targeting_webasm || targeting_native_webasm {
                // This is necessary when targeting webasm so that
                // Node.js can load the `.wasm` file.
                let previous_cwd = env::current_dir().unwrap();
                env::set_current_dir( artifact.parent().unwrap().join( "deps" ) ).unwrap();
                Some( previous_cwd )
            } else {
                None
            };

            let status = Command::new( nodejs_name ).args( test_args ).run();
            any_failure = any_failure || !status.is_ok();

            if let Some( previous_cwd ) = previous_cwd {
                env::set_current_dir( previous_cwd ).unwrap();
            }
        }
    } else {
        let app_js = Arc::new( Mutex::new( String::new() ) );
        let (tx, rx) = channel();
        let server_app_js = app_js.clone();
        let handlebars = Handlebars::new();
        let mut template_data = std::collections::BTreeMap::new();
        let arg_passthrough: Vec<_> = arg_passthrough.iter().map( |arg| arg.to_str().unwrap() ).collect();
        template_data.insert( "arguments", arg_passthrough );
        let test_index = handlebars.template_render( DEFAULT_TEST_INDEX_HTML, &template_data ).unwrap();
        let app_wasm: Arc< Mutex< Option< Vec< u8 > > > > = Arc::new( Mutex::new( None ) );
        let wasm_url = Arc::new( Mutex::new( None ) );

        let server_app_wasm = app_wasm.clone();
        let server_wasm_url = wasm_url.clone();

        let tx = Mutex::new( tx ); // Since rouille requires the Sync trait.
        let server = rouille::Server::new( "localhost:0", move |request| {
            let url = request.url();
            let response = if url == "/" || url == "index.html" {
                rouille::Response::html( test_index.clone() )
            } else if url == "/js/app.js" {
                let data = server_app_js.lock().unwrap().clone();
                rouille::Response::from_data( "application/javascript", data )
            } else if url == "/__cargo_web/print" {
                let mut data = String::new();
                request.data().unwrap().read_to_string( &mut data ).unwrap();
                println!( "{}", data );
                rouille::Response::text( "" )
            } else if url == "/__cargo_web/exit" {
                let mut status = String::new();
                request.data().unwrap().read_to_string( &mut status ).unwrap();

                let status: u32 = status.parse().unwrap();
                tx.lock().unwrap().send( status ).unwrap();
                rouille::Response::text( "" )
            } else {
                match *server_wasm_url.lock().unwrap() {
                    Some( ref wasm_url ) if url == *wasm_url => {
                        let data = server_app_wasm.lock().unwrap().as_ref().unwrap().clone();
                        rouille::Response::from_data( "application/wasm", data )
                    },
                    _ => rouille::Response::empty_404()
                }
            };

            response.with_no_cache()
        }).unwrap();

        let server_address = server.server_addr();
        thread::spawn( move || {
            server.run();
        });

        for artifact in post_artifacts_per_build {
            if targeting_webasm {
                let wasm_filename = artifact.with_extension( "wasm" ).file_name().unwrap().to_str().unwrap().to_owned();
                let wasm_path = artifact.parent().unwrap().join( "deps" ).join( &wasm_filename );
                *wasm_url.lock().unwrap() = Some( format!( "/{}", wasm_filename ) );
                *app_wasm.lock().unwrap() = Some( read_bytes( wasm_path ).unwrap() );
            }

            *app_js.lock().unwrap() = read( artifact ).unwrap();

            let tmpdir = TempDir::new( "cargo-web-chromium-profile" ).unwrap();
            let tmpdir = tmpdir.path().to_string_lossy();
            let mut command = Command::new( chromium_executable );
            command
                // TODO: Switch to headless.
                .arg( format!( "--app=http://localhost:{}", server_address.port() ) )
                .arg( "--disable-gpu" )
                .arg( "--no-first-run" )
                .arg( "--disable-restore-session-state" )
                .arg( "--no-default-browser-check" )
                .arg( "--disable-java" )
                .arg( "--disable-client-side-phishing-detection" )
                .arg( format!( "--user-data-dir={}", tmpdir ) );

            command
                .stdout( Stdio::null() )
                .stderr( Stdio::null() )
                .stdin( Stdio::null() );

            let mut child = command.spawn().unwrap();
            let start_time = Instant::now();
            let mut finished = false;
            while start_time.elapsed().as_secs() < 60 {
                match rx.recv_timeout( Duration::from_secs( 1 ) ) {
                    Ok( status ) => {
                        if status != 0 {
                            println_err!( "error: process exited with a status of {}", status );
                            any_failure = true;
                        }
                        finished = true;
                        break;
                    },
                    Err( RecvTimeoutError::Timeout ) => {
                        continue;
                    },
                    Err( RecvTimeoutError::Disconnected ) => unreachable!()
                }
            }
            if !finished {
                println_err!( "error: tests timed out!" );
                any_failure = true;
            }

            child.kill().unwrap();
            child.wait().unwrap();
        }
    }

    if any_failure {
        exit( 101 );
    } else {
        if targeting_native_webasm {
            println_err!( "All tests passed!" );
            // At least **I hope** that's the case; there are no prints
            // when running those tests, so who knows what happens. *shrug*
        }
    }

    Ok(())
}

fn command_start< 'a >( matches: &clap::ArgMatches< 'a >, project: &CargoProject ) -> Result< (), Error > {
    let use_system_emscripten = matches.is_present( "use-system-emscripten" );
    let targeting_webasm_unknknown_unknown = matches.is_present( "target-webasm" );
    let targeting_webasm = matches.is_present( "target-webasm-emscripten" ) || targeting_webasm_unknknown_unknown;
    let extra_path = if matches.is_present( "target-webasm" ) { None } else { check_for_emcc( use_system_emscripten, targeting_webasm ) };

    let build_matcher = BuildArgsMatcher {
        matches: matches,
        project: project
    };

    let package = build_matcher.package_or_default()?;
    let config = Config::load_for_package_printing_warnings( &package ).unwrap().unwrap_or_default();
    set_link_args( &config );

    let targets = build_matcher.target_or_select( package, |target| {
        target.kind == TargetKind::Bin
    })?;

    if targets.is_empty() {
        return Err(
            Error::ConfigurationError( format!( "cannot start a webserver for a crate which is a library!" ) )
        );
    }

    let target = &targets[ 0 ];
    let build_config = build_matcher.build_config( package, target, Profile::Main );

    let mut command = build_config.as_command();
    if let Some( ref extra_path ) = extra_path {
        command.append_to_path( extra_path );
    }

    run_with_broken_first_build_hack( package, &build_config, &mut command )?;

    let artifacts = build_config.potential_artifacts( &package.crate_root );

    let output_path = &artifacts[ 0 ];
    let wasm_path = output_path.with_extension( "wasm" );
    let wasm_url = format!( "/{}", wasm_path.file_name().unwrap().to_str().unwrap() );
    let mut outputs = vec![
        Output {
            path: output_path.to_owned(),
            data: read_bytes( output_path ).unwrap()
        }
    ];

    if targeting_webasm {
        outputs.push( Output {
            path: wasm_path.clone(),
            data: read_bytes( wasm_path ).unwrap(),
        });
    }

    if targeting_webasm_unknknown_unknown {
        let js_path = output_path.with_extension( "js" );
        outputs.push( Output {
            path: js_path.clone(),
            data: read_bytes( js_path ).unwrap()
        });
    }

    let outputs = Arc::new( Mutex::new( outputs ) );

    #[allow(unused_variables)]
    let watcher = monitor_for_changes_and_rebuild( &package, &target, build_config, extra_path.as_ref().map( |path| path.as_path() ), outputs.clone() );

    let crate_static_path = package.crate_root.join( "static" );
    let target_static_path = match target.kind {
        TargetKind::Example => Some( target.source_directory.join( format!( "{}-static", target.name ) ) ),
        TargetKind::Bin => Some( target.source_directory.join( "static" ) ),
        _ => None
    };

    let address = address_or_default( matches );
    let server = rouille::Server::new( &address, move |request| {
        let mut response;

        if let Some( ref target_static_path ) = target_static_path {
            response = rouille::match_assets( &request, target_static_path );
            if response.is_success() {
                return response;
            }
        }

        response = rouille::match_assets( &request, &crate_static_path );
        if response.is_success() {
            return response;
        }

        let url = request.url();
        response = if url == "/" || url == "index.html" {
            let data = target_static_path.as_ref().and_then( |path| {
                read( path.join( "index.html" ) ).ok()
            }).or_else( || {
                read( crate_static_path.join( "index.html" ) ).ok()
            });

            if let Some( data ) = data {
                rouille::Response::html( data )
            } else {
                rouille::Response::html( DEFAULT_INDEX_HTML )
            }
        } else if url == "/js/app.js" {
            let data = outputs.lock().unwrap().iter().find( |output| output.is_js() ).unwrap().data.clone();
            rouille::Response::from_data( "application/javascript", data )
        } else if url == wasm_url {
            let data = outputs.lock().unwrap().iter().find( |output| output.is_wasm() ).unwrap().data.clone();
            rouille::Response::from_data( "application/wasm", data )
        } else {
            rouille::Response::empty_404()
        };

        response.with_no_cache()
    }).unwrap();

    println_err!( "" );
    println_err!( "If you need to serve any extra files put them in the 'static' directory" );
    println_err!( "in the root of your crate; they will be served alongside your application." );
    match target.kind {
        TargetKind::Example => println_err!( "You can also put a '{}-static' directory in your 'examples' directory.", target.name ),
        TargetKind::Bin => println_err!( "You can also put a 'static' directory in your 'src' directory." ),
        _ => unreachable!()
    };
    println_err!( "" );
    println_err!( "Your application is being served at '/js/app.js'. It will be automatically" );
    println_err!( "rebuilt if you make any changes in your code." );
    println_err!( "" );
    println_err!( "You can access the web server at `http://{}`.", &address );

    server.run();
    Ok(())
}

fn main() {
    let args = {
        // To allow running both as 'cargo-web' and as 'cargo web'.
        let mut args = env::args();
        let mut filtered_args = Vec::new();
        filtered_args.push( args.next().unwrap() );

        match args.next() {
            None => {},
            #[cfg(any(unix))]
            Some( ref arg ) if filtered_args[ 0 ].ends_with( "cargo-web" ) && arg == "web" => {},
            #[cfg(any(windows))]
            Some( ref arg ) if filtered_args[ 0 ].ends_with( "cargo-web.exe" ) && arg == "web" => {},
            Some( arg ) => filtered_args.push( arg )
        }

        filtered_args.extend( args );
        filtered_args
    };

    let matches = App::new( "cargo-web" )
        .version( env!( "CARGO_PKG_VERSION" ) )
        .setting( AppSettings::SubcommandRequiredElseHelp )
        .setting( AppSettings::VersionlessSubcommands )
        .subcommand(
            SubCommand::with_name( "build" )
                .about( "Compile a local package and all of its dependencies" )
                .arg(
                    Arg::with_name( "use-system-emscripten" )
                        .long( "use-system-emscripten" )
                        .help( "Won't try to download Emscripten; will always use the system one" )
                )
                .arg(
                    Arg::with_name( "release" )
                        .long( "release" )
                        .help( "Build artifacts in release mode, with optimizations" )
                )
                .arg(
                    Arg::with_name( "target-asmjs-emscripten" )
                        .long( "target-asmjs-emscripten" )
                        .help( "Generate asmjs through Emscripten (default)" )
                        .overrides_with_all( &["target-webasm-emscripten", "target-webasm"] )
                )
                .arg(
                    Arg::with_name( "target-webasm-emscripten" )
                        .long( "target-webasm-emscripten" )
                        .help( "Generate webasm through Emscripten" )
                        .overrides_with_all( &["target-asmjs-emscripten", "target-webasm"] )
                )
                .arg(
                    Arg::with_name( "target-webasm" )
                        .long( "target-webasm" )
                        .help( "Generates webasm through Rust's native backend (HIGHLY EXPERIMENTAL!)" )
                        .overrides_with_all( &["target-asmjs-emscripten", "target-webasm-emscripten"] )
                )
                .arg(
                    Arg::with_name( "package" )
                        .short( "p" )
                        .long( "package" )
                        .help( "Package to build" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "lib" )
                        .long( "lib" )
                        .help( "Build only this package's library" )
                )
                .arg(
                    Arg::with_name( "bin" )
                        .long( "bin" )
                        .help( "Build only the specified binary" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "example" )
                        .long( "example" )
                        .help( "Build only the specified example" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "test" )
                        .long( "test" )
                        .help( "Build only the specified test target" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "bench" )
                        .long( "bench" )
                        .help( "Build only the specified benchmark target" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
        )
        .subcommand(
            SubCommand::with_name( "test" )
                .about( "Compiles and runs tests" )
                .arg(
                    Arg::with_name( "use-system-emscripten" )
                        .long( "use-system-emscripten" )
                        .help( "Won't try to download Emscripten; will always use the system one" )
                )
                .arg(
                    Arg::with_name( "no-run" )
                        .long( "no-run" )
                        .help( "Compile, but don't run tests" )
                )
                .arg(
                    Arg::with_name( "target-asmjs-emscripten" )
                        .long( "target-asmjs-emscripten" )
                        .help( "Generate asmjs through Emscripten (default)" )
                        .overrides_with_all( &["target-webasm-emscripten", "target-webasm"] )
                )
                .arg(
                    Arg::with_name( "target-webasm-emscripten" )
                        .long( "target-webasm-emscripten" )
                        .help( "Generate webasm through Emscripten" )
                        .overrides_with_all( &["target-asmjs-emscripten", "target-webasm"] )
                )
                .arg(
                    Arg::with_name( "target-webasm" )
                        .long( "target-webasm" )
                        .help( "Generates webasm through Rust's native backend (HIGHLY EXPERIMENTAL!)" )
                        .overrides_with_all( &["target-asmjs-emscripten", "target-webasm-emscripten"] )
                )
                .arg(
                    Arg::with_name( "package" )
                        .short( "p" )
                        .long( "package" )
                        .help( "Package to build" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "release" )
                        .long( "release" )
                        .help( "Build artifacts in release mode, with optimizations" )
                )
                .arg(
                    Arg::with_name( "nodejs" )
                        .long( "nodejs" )
                        .help( "Uses Node.js to run the tests" )
                )
                .arg(
                    Arg::with_name( "passthrough" )
                        .help( "-- followed by anything will pass the arguments to the test runner")
                        .multiple( true )
                        .takes_value( true )
                        .last( true )
                )
        )
        .subcommand(
            SubCommand::with_name( "start" )
                .about( "Runs an embedded web server serving the built project" )
                .arg(
                    Arg::with_name( "use-system-emscripten" )
                        .long( "use-system-emscripten" )
                        .help( "Won't try to download Emscripten; will always use the system one" )
                )
                .arg(
                    Arg::with_name( "release" )
                        .long( "release" )
                        .help( "Build artifacts in release mode, with optimizations" )
                )
                .arg(
                    Arg::with_name( "target-asmjs-emscripten" )
                        .long( "target-asmjs-emscripten" )
                        .help( "Generate asmjs through Emscripten (default)" )
                        .overrides_with_all( &["target-webasm-emscripten", "target-webasm"] )
                )
                .arg(
                    Arg::with_name( "target-webasm-emscripten" )
                        .long( "target-webasm-emscripten" )
                        .help( "Generate webasm through Emscripten" )
                        .overrides_with_all( &["target-asmjs-emscripten", "target-webasm"] )
                )
                .arg(
                    Arg::with_name( "target-webasm" )
                        .long( "target-webasm" )
                        .help( "Generates webasm through Rust's native backend (HIGHLY EXPERIMENTAL!)" )
                        .overrides_with_all( &["target-asmjs-emscripten", "target-webasm-emscripten"] )
                )
                .arg(
                    Arg::with_name( "package" )
                        .short( "p" )
                        .long( "package" )
                        .help( "Package to build" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "bin" )
                        .long( "bin" )
                        .help( "Build only the specified binary" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "example" )
                        .long( "example" )
                        .help( "Serves the specified example" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "test" )
                        .long( "test" )
                        .help( "Build only the specified test target" )
                        .value_name( "NAME" )
                        .takes_value( true )
                )
                .arg(
                    Arg::with_name( "bench" )
                        .long( "bench" )
                        .help( "Build only the specified benchmark target" )
                        .value_name( "NAME" )
                        .takes_value( true )
                ).arg(
                    Arg::with_name( "host" )
                        .long( "host" )
                        .help( "Bind the server to this address, default `localhost`")
                        .value_name( "HOST" )
                        .takes_value( true )
                ).arg(
                    Arg::with_name( "port" )
                        .long( "port" )
                        .help( "Bind the server to this port, default 8000" )
                        .value_name( "PORT" )
                        .takes_value( true )
                )
        )
        .get_matches_from( args );

    let project = CargoProject::new( None );
    let result = if let Some( matches ) = matches.subcommand_matches( "build" ) {
        command_build( matches, &project )
    } else if let Some( matches ) = matches.subcommand_matches( "test" ) {
        command_test( matches, &project )
    } else if let Some( matches ) = matches.subcommand_matches( "start" ) {
        command_start( matches, &project )
    } else {
        return;
    };

    match result {
        Ok( _ ) => {},
        Err( error ) => {
            println_err!( "error: {}", error );
            exit( 101 );
        }
    }
}
