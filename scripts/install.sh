#!/bin/bash

set -e

BRANCH=main

if [ ! -d "Horizon" ]; then
    echo "📦 Fetching Horizon repository with version: $BRANCH"
    git clone https://github.com/Far-Beyond-Dev/Horizon.git
fi

cd Horizon
git fetch
git checkout $BRANCH
git pull