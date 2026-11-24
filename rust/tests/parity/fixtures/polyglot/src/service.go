package service

type Server struct {
	name string
}

func (s Server) Handle(request string) string {
	return respond(request)
}

func respond(request string) string {
	return request + "-ok"
}

func Start(port int) string {
	s := Server{name: "main"}
	return s.Handle("port")
}
