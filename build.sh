echo "Start build the server"
if [ "$#" -eq 0 ] || [ "$#" -gt 2 ]; then
    echo "Usage: $0 run|build -d"
    exit 1
fi
if [ "$1" = "run" ]; then
    echo "Running the server"
    if [ "$2" = "-d" ]; then
        echo "Debug mode enabled"
        export RUST_LOG=debug
        cargo run --target x86_64-unknown-linux-musl
    else
        echo "Release mode enabled"
        export RUST_LOG=info
        cargo run --target x86_64-unknown-linux-musl
    fi
elif [ "$1" = "build" ]; then
    echo "Building the server"
    cargo build --release --target x86_64-unknown-linux-musl
else
    echo "Unknown command"
fi
 echo "Build the server done"

 