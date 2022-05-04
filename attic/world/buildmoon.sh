#!/bin/bash

# static compile
# fix the error "undefined reference to 'pthread_create','pthread_mutex_unlock',etc." of linux os, in which the "pthread" function is missed, fixing by adding "-lpthread" parameter. # https://blog.csdn.net/qq_35056292/article/details/70182484
c++ -std=c++11 -I../.. -I.. -g -o mkmoonworld-static ../../node/C25519.cpp ../../node/Salsa20.cpp ../../node/SHA512.cpp ../../node/Identity.cpp ../../node/Utils.cpp ../../node/InetAddress.cpp ../../osdep/OSUtils.cpp mkmoonworld.cpp -lm -lpthread -static

# dynamic compile
# c++ -std=c++11 -I../.. -I.. -g -o mkmoonworld-dynamic ../../node/C25519.cpp ../../node/Salsa20.cpp ../../node/SHA512.cpp ../../node/Identity.cpp ../../node/Utils.cpp ../../node/InetAddress.cpp ../../osdep/OSUtils.cpp mkmoonworld.cpp -lm
