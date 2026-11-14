println "Hello from Groovy ${GroovySystem.version} on ${System.getProperty('os.name')} ${System.getProperty('os.arch')}"
def xs = (1..10).toList()
println "squares: ${xs.collect { it * it }}"
println "sum of evens: ${xs.findAll { it % 2 == 0 }.sum()}"
class P { String name; int age; String toString() { "$name($age)" } }
println([new P(name: 'ada', age: 36), new P(name: 'alan', age: 41)].sort { -it.age })
println "dynamic: ${'abc'.toUpperCase().reverse()}"
