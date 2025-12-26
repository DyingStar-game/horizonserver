#!/bin/bash

set -e

REF=9aeaaa1ecc794658a728dd2399c501e2e6320b2c
if [ ! -d "Horizon" ]; then
    # echo "📦 Fetching Horizon repository with ref: $REF"
    git clone https://github.com/Far-Beyond-Dev/Horizon.git
fi

cd Horizon
git fetch
git checkout $REF
git pull origin $REF

git apply ../horizon.diff
cp ../plugins.toml . 
