#!/bin/bash

set -e

VERSION=v0.39.0

if [ ! -d "Horizon" ]; then
    echo "📦 Fetching Horizon repository with version: $VERSION"
    git clone https://github.com/Far-Beyond-Dev/Horizon.git
fi

cd Horizon
git checkout $VERSION
