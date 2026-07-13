# Baregen: Coroutine/Generator with customizations

Coroutine/Generatorを独自実装したRustのproc macroライブラリ。
既存ライブラリ(eg. genawaiter crate)と違い、`async`を使用せず独自のコード変換により関数の中断と再開を実現する。



