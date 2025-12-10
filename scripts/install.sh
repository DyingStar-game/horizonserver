#!/bin/bash

set -e

REF=46eff52740fe4485cae229f03de5c261627565bd

if [ ! -d "Horizon" ]; then
    echo "📦 Fetching Horizon repository with ref: $REF"
    git clone https://github.com/Far-Beyond-Dev/Horizon.git
fi

cd Horizon
git fetch
git checkout $REF
git pull origin $REF

git apply ../horizon.diff