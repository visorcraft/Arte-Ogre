;; Example WASM tile filter for Arte Ogre.
;;
;; Inverts the R, G and B channels of each RGBA32F pixel, leaving alpha alone.
;; The host calls `process(in_ptr, in_len, w, h, params_ptr, params_len)` and
;; reads `w*h*4` floats back from the returned pointer. wasmtime loads this
;; `.wat` text directly (its `wat` feature is enabled by default), so no
;; separate compile step is needed.
(module
    (memory (export "memory") 1)
    (func (export "process")
        (param $in i32) (param $len i32) (param $w i32) (param $h i32)
        (param $pp i32) (param $plen i32) (result i32)
        (local $i i32)
        (local $end i32)
        (local $off i32)
        ;; end = in + (len / 4) * 4  (round down to a whole float count)
        local.get $in
        local.get $len
        i32.const 4
        i32.div_u
        i32.const 4
        i32.mul
        i32.add
        local.set $end
        i32.const 0
        local.set $i
        block $done
            loop $loop
                local.get $i
                local.get $end
                i32.ge_u
                br_if $done
                ;; r = 1 - r
                local.get $in
                local.get $i
                i32.add
                local.tee $off
                f32.const 1.0
                local.get $off
                f32.load
                f32.sub
                f32.store
                ;; g = 1 - g
                local.get $off
                i32.const 4
                i32.add
                local.tee $off
                f32.const 1.0
                local.get $off
                f32.load
                f32.sub
                f32.store
                ;; b = 1 - b
                local.get $off
                i32.const 4
                i32.add
                local.tee $off
                f32.const 1.0
                local.get $off
                f32.load
                f32.sub
                f32.store
                ;; advance one RGBA pixel (16 bytes), leaving alpha untouched
                local.get $i
                i32.const 16
                i32.add
                local.set $i
                br $loop
            end
        end
        local.get $in
    )
)
